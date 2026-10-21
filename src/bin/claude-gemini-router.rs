use omarchy_claude_codex_router::server::serve;
use std::env;

#[tokio::main]
async fn main() {
    let mut host = env::var("ROUTER_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let mut port: u16 = env::var("ROUTER_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(4000);
    let mut project: Option<String> = None;
    let mut location: Option<String> = None;

    let args: Vec<String> = env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--host" => {
                if i + 1 < args.len() {
                    host = args[i + 1].clone();
                    i += 2;
                } else {
                    i += 1;
                }
            }
            "--port" => {
                if i + 1 < args.len() {
                    if let Ok(p) = args[i + 1].parse::<u16>() {
                        port = p;
                    }
                    i += 2;
                } else {
                    i += 1;
                }
            }
            "--project" => {
                if i + 1 < args.len() {
                    project = Some(args[i + 1].clone());
                    i += 2;
                } else {
                    i += 1;
                }
            }
            "--location" => {
                if i + 1 < args.len() {
                    location = Some(args[i + 1].clone());
                    i += 2;
                } else {
                    i += 1;
                }
            }
            "--help" | "-h" => {
                println!(
                    "Usage: claude-gemini-router [--host HOST] [--port PORT] [--project PROJECT] [--location LOCATION]"
                );
                return;
            }
            _ => {
                i += 1;
            }
        }
    }

    if let Err(err) = serve(&host, port, project, location).await {
        eprintln!("Router server error: {}", err);
        std::process::exit(1);
    }
}
