use toy::net::client::Client;

#[test]
fn test_client_port() {
    assert_eq!(Client::new("localhost").port_of(), 8080);
}
