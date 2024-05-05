mod common;
mod redis_db;
mod rpc;

use redis_db::RedisDB;
use std::env;

use crate::rpc::{fetch_from_rpc, RpcResultPair, RpcTask};
use dotenv::dotenv;
use tokio::sync::mpsc;

const PROJECT_ID: &str = "balances_backfill";

async fn start(blocks_sink: mpsc::Sender<Vec<String>>) {
    let export_fn = env::var("EXPORT_FN").expect("Missing env EXPORT_FN");
    let f = std::fs::File::open(export_fn).expect("Failed to open export file");
    let mut csv_reader = csv::ReaderBuilder::new()
        .delimiter(b' ')
        .has_headers(false)
        .from_reader(std::io::BufReader::new(f));

    let mut pairs = Vec::new();
    for (i, result) in csv_reader.records().enumerate() {
        let record = result.expect("Failed to read record");
        let token_id = record.get(1).expect("Missing token_id");
        let account_id = record.get(0).expect("Missing account_id");
        pairs.push(format!("{}:{}", token_id, account_id));
        if pairs.len() == 1000 {
            let mut new_pairs = vec![];
            std::mem::swap(&mut new_pairs, &mut pairs);
            blocks_sink.send(new_pairs).await.expect("Failed to send");
        }
        if i % 100000 == 0 {
            tracing::info!(target: PROJECT_ID, "Read {} records", i);
        }
    }
}

pub fn streamer() -> mpsc::Receiver<Vec<String>> {
    let (sender, receiver) = mpsc::channel(100);
    tokio::spawn(start(sender));
    receiver
}

#[tokio::main]
async fn main() {
    openssl_probe::init_ssl_cert_env_vars();
    dotenv().ok();

    common::setup_tracing("balances_backfill=info,redis=info,rpc=debug");

    tracing::log::info!(target: PROJECT_ID, "Starting Balance backfill");

    let rpc_config = rpc::RpcConfig::from_env();

    let redis_db = RedisDB::new(Some(
        env::var("WRITE_REDIS_URL").expect("Missing env WRITE_REDIS_URL"),
    ))
    .await;

    let stream = streamer();
    process_balances(stream, redis_db, &rpc_config).await;
}

async fn process_balances(
    mut stream: mpsc::Receiver<Vec<String>>,
    mut redis_db: RedisDB,
    rpc_config: &rpc::RpcConfig,
) {
    let mut total_pairs = 0;
    while let Some(pairs) = stream.recv().await {
        total_pairs += pairs.len();
        update_balances(&mut redis_db, pairs, rpc_config).await;
        tracing::info!(target: PROJECT_ID, "Processed {} pairs", total_pairs);
    }
}

async fn update_balances(redis_db: &mut RedisDB, pairs: Vec<String>, rpc_config: &rpc::RpcConfig) {
    let mut tasks = vec![];
    // Pair tasks
    tasks.extend(pairs.iter().map(|pair| {
        let (token_id, account_id) = pair.split_once(':').unwrap();
        let account_id = account_id.to_string();
        RpcTask::FtPair {
            block_height: None,
            token_id: token_id.to_string(),
            account_id: account_id.to_string(),
        }
    }));
    // Fetching balances
    let results = fetch_from_rpc(&tasks, &rpc_config)
        .await
        .expect("Failed to fetch updates from the RPC");

    // Save balances to redis
    let res: redis::RedisResult<()> = with_retries!(redis_db, |connection| async {
        let mut pipe = redis::pipe();
        for RpcResultPair { task, result } in &results {
            if result.is_none() {
                continue;
            }
            let (token_id, account_id) = match task {
                RpcTask::FtPair {
                    token_id,
                    account_id,
                    ..
                } => (token_id, account_id),
                _ => unreachable!(),
            };
            let balance = result.as_ref().unwrap().unwrap_as_ft_pair().balance;
            pipe.cmd("HSETNX")
                .arg(format!("b:{}", token_id))
                .arg(account_id)
                .arg(balance.to_string())
                .ignore();
        }

        pipe.query_async(connection).await
    });
    res.expect("Failed to update");
}
