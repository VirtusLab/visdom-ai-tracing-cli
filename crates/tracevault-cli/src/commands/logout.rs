use crate::api_client::ApiClient;
use crate::credentials::Credentials;

pub async fn logout() -> Result<(), Box<dyn std::error::Error>> {
    let creds = Credentials::load().ok_or("Not logged in. No credentials file found.")?;

    let client = ApiClient::new(&creds.server_url, Some(&creds.token));
    match client.logout().await {
        Ok(()) => {}
        Err(e) => eprintln!("Warning: could not invalidate server session: {e}"),
    }

    Credentials::delete()?;
    println!("Logged out. Credentials removed.");
    Ok(())
}
