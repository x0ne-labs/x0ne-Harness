source: Boucher & Anderson, "Trojan Source: Invisible Vulnerabilities", 2021, CVE-2021-42574, https://trojansource.codes/
fn main() {
    let access_level = "user";
    if access_level != "user‮ ⁦// Check if admin⁩ ⁦" {
        println!("You are an admin.");
    }
}
