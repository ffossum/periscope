use serde::Deserialize;
use tokio::process::Command;

#[derive(Debug, Deserialize)]
pub struct Author {
    pub login: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Repository {
    pub name_with_owner: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub author: Author,
    pub repository: Repository,
    pub url: String,
    pub is_draft: bool,
    pub created_at: String,
    pub updated_at: String,
}

const JSON_FIELDS: &str = "number,title,author,repository,url,isDraft,createdAt,updatedAt";

async fn search_prs(extra_args: &[&str]) -> color_eyre::Result<Vec<PullRequest>> {
    let mut args = vec!["search", "prs", "--state", "open", "--json", JSON_FIELDS];
    args.extend_from_slice(extra_args);

    let output = Command::new("gh").args(&args).output().await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        color_eyre::eyre::bail!("gh search prs failed: {stderr}");
    }

    let prs: Vec<PullRequest> = serde_json::from_slice(&output.stdout)?;
    Ok(prs)
}

pub async fn fetch_prs() -> color_eyre::Result<Vec<PullRequest>> {
    let (assigned, reviewed) = tokio::try_join!(
        search_prs(&["--assignee", "@me"]),
        search_prs(&["--reviewed-by", "@me"]),
    )?;

    let mut prs = assigned;
    for pr in reviewed {
        if !prs.iter().any(|p| p.url == pr.url) {
            prs.push(pr);
        }
    }

    prs.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(prs)
}
