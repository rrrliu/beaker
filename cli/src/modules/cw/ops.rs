use super::config::CWConfig;
use crate::framework::config::Account;
use crate::utils::template::Template;
use crate::{framework::Context, utils::cosmos::Client};
use anyhow::Context as _;
use anyhow::Result;
use anyhow::{anyhow, bail};
use cosmrs::cosmwasm::MsgStoreCode;
use cosmrs::rpc::endpoint::broadcast::tx_commit::Response;
use cosmrs::{
    bip32,
    crypto::secp256k1,
    tx::{self, Fee, Msg, SignDoc, SignerInfo},
    Coin,
};
use cosmrs::{dev, rpc};
use std::fs::File;
use std::io::{BufReader, Read};
use std::{env, path::PathBuf, process::Command};

pub fn new<'a, Ctx: Context<'a, CWConfig>>(
    ctx: Ctx,
    name: &str,
    version: Option<String>,
    target_dir: Option<PathBuf>,
) -> Result<()> {
    let cfg = ctx.config()?;
    let repo = &cfg.template_repo;
    let version = version.unwrap_or_else(|| "main".to_string());
    let target_dir =
        target_dir.unwrap_or(ctx.root()?.join(PathBuf::from(cfg.contract_dir.as_str())));

    let cw_template = Template::new(name.to_string(), repo.to_owned(), version, target_dir, None);
    cw_template.generate()
}

pub fn build<'a, Ctx: Context<'a, CWConfig>>(
    ctx: Ctx,
    optimize: &bool,
    aarch64: &bool,
) -> Result<()> {
    let root = ctx.root()?;

    let wp_name = root.file_name().unwrap().to_str().unwrap(); // handle properly

    env::set_current_dir(&root)?;

    let root_dir_str = root.to_str().unwrap();

    let _build = Command::new("cargo")
        .env(" RUSTFLAGS", "-C link-arg=-s")
        .arg("build")
        .arg("--release")
        .arg("--target=wasm32-unknown-unknown")
        .spawn()?
        .wait()?;

    if *optimize {
        println!("Optimizing wasm...");

        let arch_suffix = if *aarch64 { "-arm64" } else { "" };

        let _optim = Command::new("docker")
            .args(&[
                "run",
                "--rm",
                "-v",
                format!("{root_dir_str}:/code").as_str(),
                "--mount",
                format!("type=volume,source={wp_name}_cache,target=/code/target").as_str(),
                "--mount",
                "type=volume,source=registry_cache,target=/usr/local/cargo/registry",
                format!("cosmwasm/workspace-optimizer{arch_suffix}:0.12.6").as_str(), // TODO: Extract version & check for architecture
            ])
            .spawn()?
            .wait()?;
    }

    Ok(())
}
pub fn store_code<'a, Ctx: Context<'a, CWConfig>>(
    ctx: Ctx,
    contract_name: &str,
    chain_id: &str,
    gas_amount: &u64,
    gas_limit: &u64,
    timeout_height: &u32,
    signer_account: &str,
) -> Result<()> {
    let global_config = ctx.global_config()?;
    let account_prefix = global_config.account_prefix().as_str();
    let denom = global_config.denom().as_str();
    let derivation_path = global_config.derivation_path().as_str();

    let signer_priv = match global_config.accounts().get(signer_account) {
        None => bail!("signer account: `{signer_account}` is not defined"),
        Some(Account::FromMnemonic { mnemonic }) => from_mnemonic(mnemonic, derivation_path),
        Some(Account::FromPrivateKey { private_key }) => {
            Ok(secp256k1::SigningKey::from_bytes(private_key.as_bytes()).unwrap())
            // TODO: need fix
        }
    }?;

    let signer_pub = signer_priv.public_key();
    let signer_account_id = signer_pub.account_id(account_prefix).unwrap();

    let wasm = read_wasm(ctx, contract_name)?;

    // TODO: auto gas
    // https://docs.cosmos.network/main/basics/tx-lifecycle.html#gas-and-fees
    let amount = Coin {
        amount: gas_amount.to_owned().into(),
        denom: denom.parse().unwrap(),
    };
    let fee = Fee::from_amount_and_gas(amount, *gas_limit);

    let msg_store_code = MsgStoreCode {
        sender: signer_account_id.clone(),
        wasm_byte_code: wasm,
        instantiate_permission: None, // TODO: Add this when working on migration
    }
    .to_any()
    .unwrap();

    let _: Response = init_tokio_runtime().block_on(async {
        let client = Client::local(chain_id, derivation_path);
        let acc = client
            .account(signer_account_id.as_ref())
            .await
            .with_context(|| "Account can't be initialized")?;

        let tx_body = tx::Body::new(vec![msg_store_code], "", *timeout_height);
        let auth_info = SignerInfo::single_direct(Some(signer_pub), acc.sequence).auth_info(fee);
        let sign_doc = SignDoc::new(
            &tx_body,
            &auth_info,
            &chain_id.parse().unwrap(),
            acc.account_number,
        )
        .unwrap();
        let tx_raw = sign_doc.sign(&signer_priv).unwrap();

        let rpc_client = rpc::HttpClient::new(client.rpc_address().as_str()).unwrap();
        dev::poll_for_first_block(&rpc_client).await;

        let tx_commit_response = tx_raw.broadcast_commit(&rpc_client).await.unwrap();

        if tx_commit_response.check_tx.code.is_err() {
            return Err(anyhow!(
                "check_tx failed: {:?}",
                tx_commit_response.check_tx
            ));
        }

        if tx_commit_response.deliver_tx.code.is_err() {
            return Err(anyhow!(
                "deliver_tx failed: {:?}",
                tx_commit_response.deliver_tx
            ));
        }

        dbg!(&tx_commit_response);

        dev::poll_for_tx(&rpc_client, tx_commit_response.hash).await;

        anyhow::Ok(tx_commit_response)
    })?;

    Ok(())
}

fn from_mnemonic(
    phrase: &str,
    derivation_path: &str,
) -> Result<secp256k1::SigningKey, anyhow::Error> {
    let seed = bip32::Mnemonic::new(phrase, bip32::Language::English)?.to_seed("");
    let signer_priv: secp256k1::SigningKey =
        bip32::XPrv::derive_from_path(seed, &derivation_path.parse()?)
            .map(Into::into)
            .unwrap();
    Ok(signer_priv)
}

fn read_wasm<'a, Ctx: Context<'a, CWConfig>>(
    ctx: Ctx,
    contract_name: &str,
) -> Result<Vec<u8>, anyhow::Error> {
    let wasm_path = ctx
        .root()?
        .as_path()
        .join("artifacts")
        .join(format!("{contract_name}.wasm"));
    let wasm_path_str = &wasm_path.as_os_str().to_string_lossy();
    let f = File::open(&wasm_path).with_context(|| {
        format!(
            "`{wasm_path_str}` not found, please build and optimize the contract before store code`"
        )
    })?;
    let mut reader = BufReader::new(f);
    let mut wasm = Vec::new();
    reader.read_to_end(&mut wasm)?;
    Ok(wasm)
}

fn init_tokio_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}
