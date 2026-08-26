#[tokio::main]
async fn main() {
    let url = std::env::args().nth(1).unwrap();
    let c = reqwest::Client::builder()
        .user_agent("nucleusd/0.1")
        .build()
        .unwrap();
    let r = c
        .get(&url)
        .header("accept", "*/*")
        .send()
        .await
        .unwrap();
    println!("default client -> status {} final-url {}", r.status(), r.url());
    let c4 = reqwest::Client::builder()
        .user_agent("nucleusd/0.1")
        .local_address(Some(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)))
        .build()
        .unwrap();
    let r4 = c4.get(&url).send().await.unwrap();
    println!("ipv4 client    -> status {} final-url {}", r4.status(), r4.url());
    let cnp = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (X11; Linux x86_64) Gecko/20100101 Firefox/126.0")
        .build()
        .unwrap();
    let rn = cnp.get(&url).send().await.unwrap();
    println!("browser UA     -> status {}", rn.status());
}
