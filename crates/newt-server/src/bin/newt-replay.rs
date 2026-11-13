use anyhow::Context;
use newt_server::replay::{Replay, SessionRef};

const USAGE: &str = "usage: newt-replay [--url http://127.0.0.1:7878] [--token-env NEWT_TOKEN] [--from-turn N] [--turns N] <agent/key to replay> <agent/key to replay into>";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut base_url = "http://127.0.0.1:7878".to_owned();
    let mut token_env = "NEWT_TOKEN".to_owned();
    let mut from_turn = 0;
    let mut turns = None;
    let mut sessions = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--url" => base_url = args.next().context(USAGE)?,
            "--token-env" => token_env = args.next().context(USAGE)?,
            "--from-turn" => from_turn = args.next().context(USAGE)?.parse().context(USAGE)?,
            "--turns" => turns = Some(args.next().context(USAGE)?.parse().context(USAGE)?),
            _ if !arg.starts_with("--") => {
                sessions.push(arg.parse::<SessionRef>().map_err(anyhow::Error::msg)?);
            }
            _ => anyhow::bail!(USAGE),
        }
    }
    let [from, into]: [SessionRef; 2] = sessions.try_into().map_err(|_| anyhow::anyhow!(USAGE))?;
    let token = std::env::var(&token_env)
        .with_context(|| format!("environment variable {token_env} is not set"))?;
    Replay {
        base_url,
        token,
        from,
        into,
        from_turn,
        turns,
    }
    .run(&mut std::io::stdout().lock())
    .await
}
