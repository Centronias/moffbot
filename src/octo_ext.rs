#[derive(serde::Deserialize)]
pub struct RevReqs {
    pub users: Vec<octocrab::models::Author>,
    // It's just some json because I don't care to deserialize it.
    #[allow(dead_code)]
    pub teams: serde_json::Value,
}

pub async fn get_requested_reviewers(
    octo: &octocrab::Octocrab,
    org: &str,
    repo: &str,
    pr: u64,
) -> octocrab::Result<RevReqs> {
    octo.get::<RevReqs, _, u8>(
        format!("/repos/{org}/{repo}/pulls/{pr}/requested_reviewers"),
        None,
    )
    .await
}
