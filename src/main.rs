mod config;
mod media;
mod nat;
mod sip;
mod ui;

fn main() -> Result<(), slint::PlatformError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("her_sip=info,warn")),
        )
        .init();

    ui::run_ui()
}
