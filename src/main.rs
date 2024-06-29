
use ellipsis_client::tokio;
use project_2::websocket::{self, Terminal};
use std::fs::{self};
use std::process::exit;
use text_io::read;
use url::Url;
use websocket::{Market, WebSocket};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("Select network:");
    println!("1. Mainnet");
    println!("2. Devnet");
    let network_choice: String = read!();

    let (file_path, network_rpc) = match network_choice.as_str() {
        "1" => (
            "data/mainnet_markets.json",
            "api.mainnet-beta.solana.com",
        ),
        "2" => ("data/devnet_markets.json", "api.devnet.solana.com"),
        _ => {
            println!("Invalid selection, defaulting to main");
            (
                "data/mainnet_markets.json",
                "api.mainnet-beta.solana.com",
            )
        }
    };

    let markets = load_markets(file_path).expect("Failed to load markets");
    let mut terminal = Terminal {
        rpc: network_rpc.to_string(),
        market: select_market(&markets),
        websocket: WebSocket::init(
            Url::parse(&format!("wss://{}", network_rpc)).expect("Unable to parse network rpc"),
        )
        .await,
    };

    println!(
        "Market selected: {} / {}",
        terminal.market.base_ticker, terminal.market.quote_ticker
    );

    loop {
        println!(
            "Selected market: {} ({} / {})",
            terminal.market.account, terminal.market.base_ticker, terminal.market.quote_ticker
        );
        println!("Enter command:");
        println!("1. View Orderbook and Recent Trades");
        println!("2. Change Market");
        println!("Enter 'quit' to quit.");

        let command: String = read!();

        match command.as_str() {
            "1" => {
                let _ = terminal
                    .websocket
                    .subscribe_market_until_exit(
                        &terminal.market,
                        &format!("https://{}", terminal.rpc),
                    )
                    .await;
            }
            "2" => terminal.market = select_market(&markets),
            "quit" => exit(0),
            _ => println!("Unknown command."),
        }
    }
}

fn load_markets(file_path: &str) -> Result<Vec<Market>, serde_json::Error> {
    let file_contents = fs::read_to_string(file_path).expect("Failed to read file");
    serde_json::from_str(&file_contents)
}

fn select_market(markets: &[Market]) -> Market {
    println!("Select market:");
    for (index, market) in markets.iter().enumerate() {
        println!(
            "{}. {} / {}",
            index + 1,
            market.base_ticker,
            market.quote_ticker
        );
    }

    loop {
        let choice: usize = read!();
        if choice > 0 && choice <= markets.len() {
            return markets[choice - 1].clone();
        } else {
            println!("Invalid selection, please select a valid number.");
        }
    }
}
