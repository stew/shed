use std::process::ExitCode;

mod exec;
mod tui;

#[tokio::main]
async fn main() -> ExitCode {
    if let Err(e) = tui::run().await {
        eprintln!("shed: {e}");
        return ExitCode::from(1);
    }
    ExitCode::SUCCESS
}
