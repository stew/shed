use shed_core::{Filter, FilterSpec, PipelineValue, Value};
use std::process::ExitCode;

mod exec;

const DEFAULT_CAPTURE_CAP: usize = 16 * 1024 * 1024;

#[tokio::main]
async fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() {
        eprintln!("usage: shed <command> [args...]");
        return ExitCode::from(2);
    }

    let capture = match exec::run_command(&argv, DEFAULT_CAPTURE_CAP).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("shed: {e}");
            return ExitCode::from(1);
        }
    };

    let pipeline = [FilterSpec::FromLines];
    let mut value = PipelineValue::Bytes(capture.stdout);
    for f in &pipeline {
        match f.apply(value) {
            Ok(v) => value = v,
            Err(e) => {
                eprintln!("shed: filter error: {e}");
                return ExitCode::from(1);
            }
        }
    }

    if let PipelineValue::Structured(Value::List(items)) = value {
        for (i, item) in items.iter().enumerate() {
            if let Value::Record(r) = item {
                if let Some(Value::String(s)) = r.get("line") {
                    println!("{:>4}  {}", i + 1, s);
                }
            }
        }
    }

    if capture.truncated {
        eprintln!("(output truncated)");
    }
    if let Some(code) = capture.exit_code {
        if code != 0 {
            eprintln!("(exit code: {code})");
        }
    }

    ExitCode::SUCCESS
}
