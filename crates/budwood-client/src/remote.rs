//! A blocking HTTP client for the v1 API.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use budwood_core::api::{
    codes, CommitRequest, CommitResponse, CreateRepoRequest, DiffResponse, ErrorBody, IdList, LogResponse, RepoList,
    Revision, TreeResponse, MAX_BATCH,
};
use budwood_core::{Changeset, ObjectId, RepoPath};
use reqwest::blocking::{Client, RequestBuilder, Response};

/// An error response from the server.
#[derive(Debug, thiserror::Error)]
#[error("{}", .body.message)]
pub struct ApiError {
    pub status: u16,
    pub body: ErrorBody,
}

impl ApiError {
    pub fn is_conflict(&self) -> bool {
        self.body.code == codes::CONFLICT
    }

    pub fn is_not_found(&self) -> bool {
        self.body.code == codes::NOT_FOUND
    }
}

#[derive(Clone)]
pub struct Remote {
    server: String,
    repo: String,
    http: Client,
}

impl Remote {
    pub fn new(server: &str, repo: &str) -> Result<Self> {
        let server = server.trim_end_matches('/').to_string();
        if !(server.starts_with("http://") || server.starts_with("https://")) {
            bail!("server must be an http:// or https:// URL, got {server:?}");
        }
        Ok(Self { server, repo: repo.to_string(), http: http_client()? })
    }

    pub fn server(&self) -> &str {
        &self.server
    }

    pub fn repo(&self) -> &str {
        &self.repo
    }

    /// Creates a repository; returns false if it already existed.
    pub fn create_repo(server: &str, name: &str) -> Result<bool> {
        let server = server.trim_end_matches('/');
        let req = http_client()?.post(format!("{server}/v1/repos")).json(&CreateRepoRequest { name: name.to_string() });
        Ok(send(req)?.status() == reqwest::StatusCode::CREATED)
    }

    pub fn list_repos(server: &str) -> Result<Vec<String>> {
        let server = server.trim_end_matches('/');
        let list: RepoList = send(http_client()?.get(format!("{server}/v1/repos")))?.json()?;
        Ok(list.repos)
    }

    fn url(&self, rest: &str) -> String {
        format!("{}/v1/repos/{}/{}", self.server, self.repo, rest)
    }

    pub fn head(&self) -> Result<Revision> {
        Ok(send(self.http.get(self.url("head")))?.json()?)
    }

    /// Resolves a revision name: `head`, a sequence number or a changeset id.
    pub fn resolve(&self, rev: &str) -> Result<Revision> {
        Ok(self.tree(rev, &RepoPath::root())?.revision)
    }

    pub fn log(&self, path: &RepoPath, before: Option<u64>, limit: usize) -> Result<Vec<Changeset>> {
        let mut req = self.http.get(self.url("log")).query(&[("path", path.as_str()), ("limit", &limit.to_string())]);
        if let Some(before) = before {
            req = req.query(&[("before", before)]);
        }
        let resp: LogResponse = send(req)?.json()?;
        resp.changesets.into_iter().map(verified).collect()
    }

    pub fn changeset(&self, rev: &str) -> Result<Changeset> {
        verified(send(self.http.get(self.url(&format!("changesets/{rev}"))))?.json()?)
    }

    pub fn tree(&self, rev: &str, path: &RepoPath) -> Result<TreeResponse> {
        let req = self.http.get(self.url("tree")).query(&[("rev", rev), ("path", path.as_str())]);
        Ok(send(req)?.json()?)
    }

    pub fn file(&self, rev: &str, path: &RepoPath) -> Result<Vec<u8>> {
        let req = self.http.get(self.url("file")).query(&[("rev", rev), ("path", path.as_str())]);
        Ok(send(req)?.bytes()?.to_vec())
    }

    pub fn diff(&self, from: &str, to: &str, path: &RepoPath) -> Result<DiffResponse> {
        let req = self.http.get(self.url("diff")).query(&[("from", from), ("to", to), ("path", path.as_str())]);
        Ok(send(req)?.json()?)
    }

    pub fn get_object(&self, id: &ObjectId) -> Result<Vec<u8>> {
        Ok(send(self.http.get(self.url(&format!("objects/{id}"))))?.bytes()?.to_vec())
    }

    /// Fetches several objects in one round trip per [`MAX_BATCH`] ids.
    /// Callers must verify the returned bytes against their ids.
    pub fn fetch(&self, ids: &[ObjectId]) -> Result<Vec<(ObjectId, Vec<u8>)>> {
        let mut out = Vec::with_capacity(ids.len());
        for chunk in ids.chunks(MAX_BATCH) {
            let req = self.http.post(self.url("objects/fetch")).json(&IdList { ids: chunk.to_vec() });
            let body = send(req)?.bytes()?;
            parse_frames(&body, &mut out)?;
        }
        Ok(out)
    }

    pub fn missing(&self, ids: &[ObjectId]) -> Result<Vec<ObjectId>> {
        let mut missing = Vec::new();
        for chunk in ids.chunks(MAX_BATCH) {
            let req = self.http.post(self.url("objects/missing")).json(&IdList { ids: chunk.to_vec() });
            let resp: IdList = send(req)?.json()?;
            missing.extend(resp.ids);
        }
        Ok(missing)
    }

    pub fn put_blob(&self, id: &ObjectId, data: Vec<u8>) -> Result<()> {
        send(self.http.put(self.url(&format!("objects/{id}"))).body(data))?;
        Ok(())
    }

    pub fn commit(&self, req: &CommitRequest) -> Result<CommitResponse> {
        let resp: CommitResponse = send(self.http.post(self.url("commit")).json(req))?.json()?;
        if !resp.changeset.verify() {
            bail!("server returned changeset {} with a mismatched id", resp.changeset.seq);
        }
        Ok(resp)
    }
}

fn http_client() -> Result<Client> {
    Client::builder()
        .user_agent(concat!("bud/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(600))
        .build()
        .context("building HTTP client")
}

fn send(req: RequestBuilder) -> Result<Response> {
    let resp = req.send().context("request failed")?;
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let text = resp.text().unwrap_or_default();
    let body = serde_json::from_str::<ErrorBody>(&text).unwrap_or_else(|_| ErrorBody {
        code: codes::INTERNAL.to_string(),
        message: format!("HTTP {status}: {}", text.trim()),
        paths: vec![],
        missing: vec![],
    });
    Err(ApiError { status: status.as_u16(), body }.into())
}

fn verified(cs: Changeset) -> Result<Changeset> {
    if !cs.verify() {
        bail!("changeset {} failed id verification", cs.seq);
    }
    Ok(cs)
}

fn parse_frames(mut body: &[u8], out: &mut Vec<(ObjectId, Vec<u8>)>) -> Result<()> {
    while !body.is_empty() {
        if body.len() < 40 {
            bail!("truncated object frame");
        }
        let id = ObjectId::from_bytes(body[..32].try_into().unwrap());
        let len = u64::from_le_bytes(body[32..40].try_into().unwrap()) as usize;
        let rest = &body[40..];
        if rest.len() < len {
            bail!("truncated object {id}");
        }
        out.push((id, rest[..len].to_vec()));
        body = &rest[len..];
    }
    Ok(())
}
