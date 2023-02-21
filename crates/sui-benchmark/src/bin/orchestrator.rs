use color_eyre::eyre::{Context, Result};
use sui_benchmark::orchestrator::{client::VultrClient, settings::Settings, testbed::Testbed};

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    let path = "crates/sui-benchmark/src/orchestrator/assets/settings.json";
    let settings = Settings::load(path).wrap_err("Failed to load settings")?;

    let token = settings.load_token()?;
    let client = VultrClient::new(token, settings.clone());

    let public_key = settings.load_ssh_public_key()?;
    client.upload_key(public_key).await?;

    let mut testbed = Testbed::new(settings, client)
        .await
        .wrap_err("Failed to crate testbed")?;

    // testbed
    //     .populate(2)
    //     .await
    //     .wrap_err("Failed to populate tested")?;

    // testbed.stop().await.wrap_err("Failed to stop tested")?;

    // testbed
    //     .install()
    //     .await
    //     .wrap_err("Failed to install software on instances")?;

    // testbed
    //     .update()
    //     .await
    //     .wrap_err("Failed to install software on instances")?;

    // testbed
    //     .configure()
    //     .await
    //     .wrap_err("Failed to install software on instances")?;

    testbed.info();
    Ok(())
}