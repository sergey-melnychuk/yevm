use serde::{Serialize, de::DeserializeOwned};

pub struct Http(reqwest::Client);

impl Default for Http {
    fn default() -> Self {
        Self::new()
    }
}

impl Http {
    pub fn new() -> Self {
        Self(reqwest::Client::new())
    }

    pub async fn post<Q: Serialize, R: DeserializeOwned>(
        &self,
        url: &str,
        body: &Q,
    ) -> eyre::Result<R> {
        let response = self.0.post(url).json(body).send().await?.json().await?;
        Ok(response)
    }
}
