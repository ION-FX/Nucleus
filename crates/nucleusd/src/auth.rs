use rand::Rng;

/// Random hyphen-grouped alphanumeric password for SFTP credentials.
pub fn generate_password() -> String {
    const CHARS: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";
    let mut rng = rand::thread_rng();
    let raw: String = (0..24)
        .map(|i| {
            let mut s = String::new();
            if i > 0 && i % 8 == 0 {
                s.push('-');
            }
            s.push(CHARS[rng.gen_range(0..CHARS.len()) as usize] as char);
            s
        })
        .collect();
    raw
}
