use std::path::PathBuf;

use anyhow::Context;
use common::{
    config::global_config,
    db::{drop_db_if_exists, init_db, migrate_db},
    logger,
    spinner::Spinner,
};
use xshell::Shell;

use super::args::genesis::GenesisArgsFinal;
use crate::{
    commands::hyperchain::args::genesis::GenesisArgs,
    configs::{
        update_general_config, update_secrets, DatabasesConfig, EcosystemConfig, HyperchainConfig,
    },
    server::{RunServer, ServerMode},
};

const SERVER_MIGRATIONS: &str = "core/lib/dal/migrations";
const PROVER_MIGRATIONS: &str = "prover/prover_dal/migrations";

pub async fn run(args: GenesisArgs, shell: &Shell) -> anyhow::Result<()> {
    let hyperchain_name = global_config().hyperchain_name.clone();
    let ecosystem_config = EcosystemConfig::from_file(shell)?;
    let hyperchain_config = ecosystem_config
        .load_hyperchain(hyperchain_name)
        .context("Hyperchain not initialized. Please create a hyperchain first")?;
    let args = args.fill_values_with_prompt(&hyperchain_config);

    genesis(args, shell, &hyperchain_config, &ecosystem_config).await?;
    logger::outro("Genesis completed successfully");

    Ok(())
}

pub async fn genesis(
    args: GenesisArgsFinal,
    shell: &Shell,
    config: &HyperchainConfig,
    ecosystem_config: &EcosystemConfig,
) -> anyhow::Result<()> {
    // Clean the rocksdb
    shell.remove_path(&config.rocks_db_path)?;
    shell.create_dir(&config.rocks_db_path)?;

    let db_config = args
        .databases_config()
        .context("Database config was not fully generated")?;
    update_general_config(shell, config)?;
    update_secrets(shell, config, &db_config, ecosystem_config)?;

    logger::note(
        "Selected config:",
        logger::object_to_string(serde_json::json!({
            "hyperchain_config": config,
            "db_config": db_config,
        })),
    );
    logger::info("Starting genesis process");

    let spinner = Spinner::new("Initializing databases...");
    initialize_databases(
        shell,
        db_config,
        config.link_to_code.clone(),
        args.dont_drop,
    )
    .await?;
    spinner.finish();

    let spinner = Spinner::new("Running server genesis...");
    run_server_genesis(config, shell)?;
    spinner.finish();

    Ok(())
}

async fn initialize_databases(
    shell: &Shell,
    db_config: DatabasesConfig,
    link_to_code: PathBuf,
    dont_drop: bool,
) -> anyhow::Result<()> {
    let path_to_server_migration = link_to_code.join(SERVER_MIGRATIONS);

    if global_config().verbose {
        logger::debug("Initializing server database")
    }
    if !dont_drop {
        drop_db_if_exists(&db_config.server.base_url, &db_config.server.database_name)
            .await
            .context("Failed to drop server database")?;
        init_db(&db_config.server.base_url, &db_config.server.database_name).await?;
    }
    migrate_db(
        shell,
        path_to_server_migration,
        &db_config.server.full_url(),
    )
    .await?;

    if global_config().verbose {
        logger::debug("Initializing prover database")
    }
    if !dont_drop {
        drop_db_if_exists(&db_config.prover.base_url, &db_config.prover.database_name)
            .await
            .context("Failed to drop prover database")?;
        init_db(&db_config.prover.base_url, &db_config.prover.database_name).await?;
    }
    let path_to_prover_migration = link_to_code.join(PROVER_MIGRATIONS);
    migrate_db(
        shell,
        path_to_prover_migration,
        &db_config.prover.full_url(),
    )
    .await?;

    Ok(())
}

fn run_server_genesis(hyperchain_config: &HyperchainConfig, shell: &Shell) -> anyhow::Result<()> {
    let server = RunServer::new(None, hyperchain_config);
    server.run(shell, ServerMode::Genesis)
}
