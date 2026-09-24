//! Thin Google OAuth 2.0 + Drive REST client.
//!
//! Deliberately hand-rolled against Google's two HTTP endpoints rather than pulling in
//! the `oauth2` crate's async client: the workspace pins `oauth2` with
//! `default-features = false`, so its built-in HTTP transport isn't available here, and
//! Google's token endpoint is a plain form POST that doesn't need a client library.

use std::collections::BTreeMap;

use anyhow::{Context, bail};
use compact_str::CompactString;
use serde::{Deserialize, Serialize};

pub const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub const USERINFO_URL: &str = "https://www.googleapis.com/oauth2/v3/userinfo";
pub const DRIVE_FILES_URL: &str = "https://www.googleapis.com/drive/v3/files";
pub const DRIVE_UPLOAD_URL: &str = "https://www.googleapis.com/upload/drive/v3/files";
pub const DRIVE_ABOUT_URL: &str = "https://www.googleapis.com/drive/v3/about";

/// Marker that travels inside an error's text when Google rejects a *refresh* grant
/// with `invalid_grant`. That answer means the stored grant is dead - revoked, expired,
/// or access removed in the user's Google account - and only reconnecting can revive
/// it, so `tokens` greps for this marker to flag the connection instead of every call
/// site parsing Google's JSON error body itself.
pub const INVALID_GRANT: &str = "invalid_grant";

/// `appProperties` key stamped on every upload with the server it belongs to. Retention
/// prunes by this tag rather than by filename prefix, so it works no matter which
/// folder a file ends up in - flat layout, per-server subfolder, or a layout changed
/// between versions. `appProperties` (plural of *app*) is the app-private namespace
/// under the `drive.file` scope, so the tag is invisible outside this extension.
pub const SERVER_PROPERTY: &str = "dev_caloptreyx_gdrive_server";
/// Companion tag with the backup's UUID, for "which panel backup is this file?" lookups.
pub const BACKUP_PROPERTY: &str = "dev_caloptreyx_gdrive_backup";

/// A deliberately unsatisfiable authorization code for the admin credential probe.
pub const PROBE_CODE: &str = "calagopus-credential-probe";

/// `drive.file` is the least-privilege scope that works: Google only exposes files the
/// app created (or that the user opened with the app), which is exactly the folder and
/// the backups we write. `openid email` is only there so the panel can show *which*
/// Google account got linked.
pub const SCOPES: &str = "openid email https://www.googleapis.com/auth/drive.file";

#[derive(Debug, Deserialize)]
pub struct TokenResponse {
    pub access_token: CompactString,
    #[serde(default)]
    pub refresh_token: Option<CompactString>,
    #[serde(default)]
    pub scope: Option<CompactString>,
}

#[derive(Debug, Deserialize)]
pub struct Userinfo {
    #[serde(default)]
    pub email: Option<CompactString>,
}

#[derive(Debug, Deserialize)]
pub struct DriveFile {
    pub id: CompactString,
}

/// Body of the resumable-upload initiation. The `MediaFileBody` below is what actually
/// streams the bytes in the follow-up `Content-Range` PUT.
#[derive(Debug, Serialize)]
pub struct ResumableInit {
    pub name: CompactString,
    pub parents: Vec<CompactString>,
    /// Server/backup identity stamped on the file at creation (see `SERVER_PROPERTY`).
    #[serde(rename = "appProperties", skip_serializing_if = "BTreeMap::is_empty")]
    pub app_properties: BTreeMap<CompactString, CompactString>,
}

/// Build the browser redirect the user approves on Google's side.
pub fn authorize_url(
    client_id: &str,
    redirect_uri: &str,
    state: &str,
    login_hint: Option<&str>,
) -> String {
    let mut url = format!(
        "{AUTH_URL}?client_id={}&redirect_uri={}&response_type=code&scope={}&state={}&access_type=offline&prompt=consent",
        urlencoding::encode(client_id),
        urlencoding::encode(redirect_uri),
        urlencoding::encode(SCOPES),
        urlencoding::encode(state),
    );

    if let Some(hint) = login_hint {
        url.push_str("&login_hint=");
        url.push_str(&urlencoding::encode(hint));
    }

    url
}

/// Exchange an authorization code for tokens.
pub async fn token_request(
    client: &reqwest::Client,
    client_id: &str,
    client_secret: &str,
    redirect_uri: &str,
    code: String,
) -> Result<TokenResponse, anyhow::Error> {
    let form = [
        ("client_id", client_id),
        ("client_secret", client_secret),
        ("grant_type", "authorization_code"),
        ("code", &code),
        ("redirect_uri", redirect_uri),
    ];

    post_form(client, &form).await
}

/// Mint a fresh access token from a stored refresh token.
///
/// Called once per push rather than tracking `expires_in`: access tokens last an hour,
/// pushes are far less frequent than that, and not storing them keeps the only durable
/// credential to the refresh token we already encrypt.
pub async fn refresh_access_token(
    client: &reqwest::Client,
    client_id: &str,
    client_secret: &str,
    refresh_token: &str,
) -> Result<TokenResponse, anyhow::Error> {
    let form = [
        ("client_id", client_id),
        ("client_secret", client_secret),
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
    ];

    post_form(client, &form).await
}

async fn post_form(
    client: &reqwest::Client,
    form: &[(&str, &str)],
) -> Result<TokenResponse, anyhow::Error> {
    let response = client.post(TOKEN_URL).form(form).send().await?;

    // Google reports token failures as HTTP errors *with a JSON body*, and
    // `error_for_status()` throws that body away - which is exactly where the one
    // signal that distinguishes "dead grant" from "transient trouble" lives.
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();

        if body.contains(INVALID_GRANT) {
            bail!("{INVALID_GRANT}: google refused the stored grant ({status}): {body}");
        }

        bail!("google token endpoint returned {status}: {body}");
    }

    Ok(response.json::<TokenResponse>().await?)
}

/// What a probe round-trip through the token endpoint says about the operator's
/// stored credentials. The probe exchanges a deliberately invalid authorization code:
/// Google validates the client id/secret first, then the redirect URI, and only then
/// the code - so each rejection narrows down which piece of the setup is wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialProbe {
    /// Everything except a real consent is wired up right: client id, secret and the
    /// registered redirect URI all matched, and only the dummy code was refused.
    Valid,
    /// The client id or secret is wrong.
    BadClient,
    /// The redirect URI is not registered on the OAuth client in the Google console -
    /// the single most common setup mistake, and the one this probe exists to catch.
    RedirectMismatch,
    /// Anything else; the caller shows Google's reply verbatim.
    Other,
}

impl CredentialProbe {
    pub fn is_valid(self) -> bool {
        self == Self::Valid
    }
}

/// Classify one token-endpoint answer for the credential probe. Kept pure so the
/// mapping from Google's error vocabulary to a diagnosis is testable without a network.
pub fn classify_probe(status: u16, body: &str) -> CredentialProbe {
    if body.contains(INVALID_GRANT) {
        CredentialProbe::Valid
    } else if body.contains("redirect_uri_mismatch") {
        CredentialProbe::RedirectMismatch
    } else if body.contains("\"invalid_client\"")
        || body.contains("\"unauthorized_client\"")
        || status == 401
    {
        CredentialProbe::BadClient
    } else {
        CredentialProbe::Other
    }
}

/// Exchange a dummy authorization code against the stored credentials and report what
/// Google's refusal says about them. Never touches a user account: no consent is
/// involved, and the code can never be valid.
pub async fn probe_credentials(
    client: &reqwest::Client,
    client_id: &str,
    client_secret: &str,
    redirect_uri: &str,
) -> Result<(CredentialProbe, String), anyhow::Error> {
    let form = [
        ("client_id", client_id),
        ("client_secret", client_secret),
        ("grant_type", "authorization_code"),
        ("code", PROBE_CODE),
        ("redirect_uri", redirect_uri),
    ];

    let response = client.post(TOKEN_URL).form(&form).send().await?;
    let status = response.status().as_u16();
    let body = response.text().await.unwrap_or_default();

    Ok((classify_probe(status, &body), body))
}

pub async fn userinfo(
    client: &reqwest::Client,
    access_token: &str,
) -> Result<Userinfo, anyhow::Error> {
    let response = client
        .get(USERINFO_URL)
        .bearer_auth(access_token)
        .send()
        .await?
        .error_for_status()?;

    Ok(response.json::<Userinfo>().await?)
}

/// `storageQuota` from `about.get` - the numbers behind the quota badge.
///
/// `usage` is what counts against `limit`; both are int64-as-string like every other
/// Drive number, hence the shared deserializer. A `null` limit means an unlimited
/// (Workspace) account, which the badge renders as usage alone.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct StorageQuota {
    #[serde(default, deserialize_with = "deserialize_opt_i64")]
    pub limit: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_opt_i64")]
    pub usage: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_opt_i64")]
    pub usage_in_drive: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AboutResponse {
    #[serde(default)]
    storage_quota: Option<StorageQuota>,
}

/// The linked account's storage quota. `about.get` is on the `drive.file` scope's
/// method list, so no scope bump is needed to show it.
pub async fn about(
    client: &reqwest::Client,
    access_token: &str,
) -> Result<StorageQuota, anyhow::Error> {
    let response = client
        .get(DRIVE_ABOUT_URL)
        .query(&[("fields", "storageQuota(limit,usage,usageInDrive)")])
        .bearer_auth(access_token)
        .send()
        .await?
        .error_for_status()?;

    let about: AboutResponse = response.json().await?;

    Ok(about.storage_quota.unwrap_or_default())
}

/// Create the folder that collects this user's backups. Returns its file id.
pub async fn create_folder(
    client: &reqwest::Client,
    access_token: &str,
    name: &str,
    parent_id: Option<&str>,
) -> Result<CompactString, anyhow::Error> {
    let mut body = serde_json::json!({
        "name": name,
        "mimeType": "application/vnd.google-apps.folder",
    });

    if let Some(parent) = parent_id {
        body["parents"] = serde_json::json!([parent]);
    }

    let response = client
        .post(DRIVE_FILES_URL)
        // `supportsAllDrives` on every call: harmless for My Drive, required the moment
        // the destination sits in a shared drive.
        .query(&[("fields", "id,name"), ("supportsAllDrives", "true")])
        .bearer_auth(access_token)
        .json(&body)
        .send()
        .await?
        .error_for_status()?;

    let file: DriveFile = response.json().await?;

    Ok(file.id)
}

/// One page of the linked backup folder.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileList {
    /// `#[serde(default)]` so an answer without the key - which Google sends when a
    /// query matches nothing - reads as an empty page instead of failing the listing.
    #[serde(default)]
    pub files: Vec<FileMetadata>,
    #[serde(default)]
    pub next_page_token: Option<CompactString>,
}

/// What `files.get` reports about one file: the folders it sits in and what it is.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileRef {
    #[serde(default)]
    pub parents: Option<Vec<CompactString>>,
    #[serde(default)]
    pub mime_type: Option<CompactString>,
}

/// A single file as `files.list` describes it.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileMetadata {
    pub id: CompactString,
    pub name: CompactString,
    /// Google encodes `int64` as a JSON string, so `size` arrives quoted.
    #[serde(default, deserialize_with = "deserialize_opt_i64")]
    pub size: Option<i64>,
    #[serde(default)]
    pub created_time: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub modified_time: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub mime_type: Option<CompactString>,
    /// The app-private identity tags, when the projection asked for them (`list_files`
    /// does so the Files table can offer Restore; `list_server_files` doesn't need them).
    #[serde(default)]
    pub app_properties: Option<BTreeMap<CompactString, CompactString>>,
}

fn deserialize_opt_i64<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    // Google types `int64` as a JSON string, so a size arrives quoted - but a bare
    // number should not fail an entire page of files over a representation difference,
    // which is what reading `Option<String>` alone would do.
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum RawSize {
        Number(i64),
        Text(String),
    }

    Ok(match Option::<RawSize>::deserialize(deserializer)? {
        None => None,
        Some(RawSize::Number(value)) => Some(value),
        Some(RawSize::Text(value)) => Some(value.parse::<i64>().map_err(serde::de::Error::custom)?),
    })
}

/// The server and backup a pushed file is stamped with, when it is stamped at all.
///
/// `None` for anything untagged or malformed, never an error: this runs over arbitrary
/// `files.list` rows, where a user-made folder (no tags) or a hand-dropped file (half
/// the tags, uuids someone typed) is normal, not exceptional. Callers turn `None` into
/// "no Restore button on this row".
pub fn backup_identity(
    app_properties: Option<&BTreeMap<CompactString, CompactString>>,
) -> Option<(uuid::Uuid, uuid::Uuid)> {
    let properties = app_properties?;
    let server_uuid = properties.get(SERVER_PROPERTY)?.parse().ok()?;
    let backup_uuid = properties.get(BACKUP_PROPERTY)?.parse().ok()?;

    Some((server_uuid, backup_uuid))
}

/// Page through the files sitting in the linked backup folder.
///
/// `drive.file` already scopes the query: Google only returns files this app created,
/// which is exactly the folder and the archives pushed into it. `createdTime desc` puts
/// the newest backups first, which is what someone checking "did my last push land?"
/// wants to see.
pub async fn list_files(
    client: &reqwest::Client,
    access_token: &str,
    folder_id: &str,
    page_token: Option<&str>,
) -> Result<FileList, anyhow::Error> {
    const PAGE_SIZE: usize = 50;

    let mut request = client
        .get(DRIVE_FILES_URL)
        .bearer_auth(access_token)
        .query(&[
            (
                "q",
                // `trashed = false` is load-bearing: Drive v3's `files.list` returns
                // trashed entries too, and without this the Files page counts every
                // copy retention pruned (they sit in the trash for thirty days) as if
                // it were still live - so "keep last 2" showed 7 files.
                format!(
                    "'{}' in parents and trashed = false",
                    folder_id.replace('\'', "\\'")
                ),
            ),
            (
                "fields",
                "nextPageToken,files(id,name,size,createdTime,modifiedTime,mimeType,appProperties)"
                    .to_string(),
            ),
            ("pageSize", PAGE_SIZE.to_string()),
            ("orderBy", "createdTime desc".to_string()),
            // Required for the listing to include anything that lives in a shared drive.
            ("supportsAllDrives", "true".to_string()),
            ("includeItemsFromAllDrives", "true".to_string()),
        ]);

    if let Some(token) = page_token {
        request = request.query(&[("pageToken", token)]);
    }

    let response = request.send().await?.error_for_status()?;

    Ok(response.json::<FileList>().await?)
}

/// Read which folders a file is currently in.
///
/// Every route that acts on a single file has to confirm the file is in *the caller's*
/// folder first. `drive.file` only narrows visibility to files this app created, and the
/// app is one OAuth client shared by every linked panel user - so without this check a
/// user who knew another user's file id could act on it from their own account.
///
/// The mime type travels along so callers can tell a folder from an archive: a file may
/// now sit one level down in a per-server subfolder (hence parents alone no longer being
/// the whole ownership check - callers walk the chain), and trashing a folder takes
/// every backup inside it, which is a different and much larger action than one file.
pub async fn file_ref(
    client: &reqwest::Client,
    access_token: &str,
    file_id: &str,
) -> Result<FileRef, anyhow::Error> {
    let response = client
        .get(format!("{DRIVE_FILES_URL}/{file_id}"))
        .query(&[
            ("fields", "parents,mimeType"),
            ("supportsAllDrives", "true"),
        ])
        .bearer_auth(access_token)
        .send()
        .await?
        .error_for_status()?;

    Ok(response.json::<FileRef>().await?)
}

/// Everything a restore needs to know about one file: identity tags, name, size, trash
/// state - read in a single `files.get`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveFileProps {
    pub id: CompactString,
    pub name: CompactString,
    #[serde(default, deserialize_with = "deserialize_opt_i64")]
    pub size: Option<i64>,
    #[serde(default)]
    pub mime_type: Option<CompactString>,
    #[serde(default)]
    pub trashed: Option<bool>,
    #[serde(default)]
    pub app_properties: Option<BTreeMap<CompactString, CompactString>>,
}

/// Read a single file's properties - what the restore route checks before it believes
/// a file id someone clicked: tagged, untrashed, this server's, this backup's.
pub async fn file_props(
    client: &reqwest::Client,
    access_token: &str,
    file_id: &str,
) -> Result<DriveFileProps, anyhow::Error> {
    let response = client
        .get(format!("{DRIVE_FILES_URL}/{file_id}"))
        .query(&[
            ("fields", "id,name,size,mimeType,trashed,appProperties"),
            ("supportsAllDrives", "true"),
        ])
        .bearer_auth(access_token)
        .send()
        .await?
        .error_for_status()?;

    Ok(response.json::<DriveFileProps>().await?)
}

/// Stream a file's raw bytes (`alt=media`) - the archive route's read half.
///
/// The response is returned rather than buffered on purpose: archives are far larger
/// than the panel should hold in memory, and Wings is streaming them onward anyway.
/// The caller runs `error_for_status` before handing the body to Wings, because Wings'
/// S3-restore fetch never checks the status itself and would happily extract an error
/// page.
pub async fn download_file(
    client: &reqwest::Client,
    access_token: &str,
    file_id: &str,
) -> Result<reqwest::Response, anyhow::Error> {
    Ok(client
        .get(format!("{DRIVE_FILES_URL}/{file_id}"))
        .query(&[("alt", "media"), ("supportsAllDrives", "true")])
        .bearer_auth(access_token)
        .send()
        .await?
        .error_for_status()?)
}

/// Find the named folder directly under `parent`, if it exists - the per-server
/// folder lookup. Returns its file id.
pub async fn find_folder(
    client: &reqwest::Client,
    access_token: &str,
    parent_id: &str,
    name: &str,
) -> Result<Option<CompactString>, anyhow::Error> {
    let q = format!(
        "'{}' in parents and name = '{}' and mimeType = 'application/vnd.google-apps.folder' and trashed = false",
        escape_query(parent_id),
        escape_query(name),
    );

    let response = client
        .get(DRIVE_FILES_URL)
        .bearer_auth(access_token)
        .query(&[
            ("q", q),
            // `name` is not optional in `FileMetadata`, so a projection of `id` alone
            // decodes as an error ("missing field `name`") - this lookup runs on every
            // push after the first, so it has to ask for both required fields.
            ("fields", "files(id,name)".to_string()),
            ("pageSize", "1".to_string()),
            ("supportsAllDrives", "true".to_string()),
            ("includeItemsFromAllDrives", "true".to_string()),
        ])
        .send()
        .await?
        .error_for_status()?;

    let list: FileList = response.json().await?;

    Ok(list.files.into_iter().next().map(|file| file.id))
}

/// Escape a value for inclusion in a Drive search query: backslashes first, then the
/// single quotes that delimit string literals.
fn escape_query(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\'', "\\'")
}

/// Every archive stamped as belonging to one server, newest first.
///
/// Matched on the `appProperties` tag rather than a filename prefix, so a server whose
/// name prefixes another's (`Goon` vs `Goon 2`) never bleeds into its neighbour's
/// copies, and so the answer spans every folder layout the files may sit in. Pages
/// through the whole result - retention needs the complete set to know what is oldest.
pub async fn list_server_files(
    client: &reqwest::Client,
    access_token: &str,
    server_uuid: &str,
) -> Result<Vec<FileMetadata>, anyhow::Error> {
    // The uuid is query-safe by construction (no quotes or backslashes), but it goes
    // through the same escaper as everything else rather than being trusted inline.
    let q = format!(
        "appProperties has {{ key='{SERVER_PROPERTY}' and value='{}' }} and trashed = false",
        escape_query(server_uuid),
    );

    let mut files = Vec::new();
    let mut page_token: Option<CompactString> = None;

    // Bounded at 20 pages (2 000 files): a runaway loop against Google's API is worse
    // than missing a prune on a pathological account, and no server accumulates that
    // many Drive copies under one uuid without someone noticing.
    for _ in 0..20 {
        let mut request = client
            .get(DRIVE_FILES_URL)
            .bearer_auth(access_token)
            .query(&[
                ("q", q.clone()),
                (
                    "fields",
                    "nextPageToken,files(id,name,createdTime)".to_string(),
                ),
                ("pageSize", "100".to_string()),
                ("orderBy", "createdTime desc".to_string()),
                ("supportsAllDrives", "true".to_string()),
                ("includeItemsFromAllDrives", "true".to_string()),
            ]);

        if let Some(token) = page_token.as_deref() {
            request = request.query(&[("pageToken", token)]);
        }

        let list: FileList = request
            .send()
            .await?
            .error_for_status()
            .context("listing the server's files in drive")?
            .json()
            .await?;

        page_token = list.next_page_token;
        let exhausted = page_token.is_none();
        files.extend(list.files);

        if exhausted {
            break;
        }
    }

    Ok(files)
}

/// Move a file to Drive's trash. A `404`/`410` counts as success: what the caller asked
/// for is "gone", and it already is.
///
/// Deliberately a `PATCH trashed = true` rather than `files.delete`: the v3 delete
/// method is documented as "*permanently deletes a file ... without moving it to the
/// trash*", which would make every caller here - the cleanup listener, the Files table's
/// delete button, retention pruning - a permanent erasure of the only off-site copy
/// while their logs, help text and modals all promise a recoverable trash.
pub async fn delete_file(
    client: &reqwest::Client,
    access_token: &str,
    file_id: &str,
) -> Result<(), anyhow::Error> {
    let response = client
        .patch(format!("{DRIVE_FILES_URL}/{file_id}"))
        .query(&[("supportsAllDrives", "true"), ("fields", "id,trashed")])
        .bearer_auth(access_token)
        .json(&serde_json::json!({ "trashed": true }))
        .send()
        .await?;

    match response.status().as_u16() {
        200 | 404 | 410 => Ok(()),
        status => Err(anyhow::anyhow!(
            "google refused to trash the file ({status}): {}",
            response.text().await.unwrap_or_default()
        )),
    }
}

/// Begin a resumable upload and return the session URI to stream the body to.
///
/// Google returns it in the `Location` header; no bytes move during this call, so it's
/// cheap to retry on its own if the panel restarts mid-push.
#[allow(clippy::too_many_arguments)]
pub async fn start_resumable_upload(
    client: &reqwest::Client,
    access_token: &str,
    name: &str,
    parent_id: &str,
    mime_type: &str,
    length: u64,
    server_uuid: &str,
    backup_uuid: &str,
) -> Result<CompactString, anyhow::Error> {
    // Identity stamped into the file itself: retention prunes on the server tag and the
    // backup tag answers "which panel backup is this file?" without a DB lookup.
    let mut app_properties = BTreeMap::new();
    app_properties.insert(
        CompactString::from(SERVER_PROPERTY),
        CompactString::from(server_uuid),
    );
    app_properties.insert(
        CompactString::from(BACKUP_PROPERTY),
        CompactString::from(backup_uuid),
    );

    let body = ResumableInit {
        name: name.into(),
        parents: vec![parent_id.into()],
        app_properties,
    };

    let response = client
        .post(DRIVE_UPLOAD_URL)
        .query(&[
            ("uploadType", "resumable"),
            ("fields", "id,name,size,webViewLink"),
            // Required when the destination folder lives in a shared drive.
            ("supportsAllDrives", "true"),
        ])
        .bearer_auth(access_token)
        .header("X-Upload-Content-Type", mime_type)
        .header("X-Upload-Content-Length", length.to_string())
        .json(&body)
        .send()
        .await?
        .error_for_status()?;

    response
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .map(CompactString::from)
        .ok_or_else(|| anyhow::anyhow!("google did not return an upload session uri"))
}

/// Outcome of asking Google how much of an in-progress upload it has committed.
pub enum UploadStatus {
    /// The session is gone (expired or unknown) - the upload has to start over.
    Expired,
    /// Google already has the whole file; the body carries its metadata.
    Complete(DriveFile),
    /// Google has committed this many leading bytes of the file.
    Committed(u64),
}

/// Ask Google for the status of an in-progress resumable upload.
///
/// A `308` means "still going"; the `Range` header says how much arrived. `200`/`201`
/// mean it finished earlier than we saw (a response we never received, for instance).
/// `404`/`410` mean the session expired and must be recreated.
/// Ask Google for the status of an in-progress resumable upload.
///
/// The bearer token matters on this `PUT` too: it is how a resume that happens long after
/// the session was opened stays attributable, and without it a status query can come back
/// `401` and read as an unexpected failure rather than the token being stale.
pub async fn query_upload_status(
    client: &reqwest::Client,
    access_token: &str,
    session_uri: &str,
    total_length: u64,
) -> Result<UploadStatus, anyhow::Error> {
    let response = client
        .put(session_uri)
        .bearer_auth(access_token)
        .header("Content-Range", format!("bytes */{total_length}"))
        .send()
        .await?;

    match response.status().as_u16() {
        308 => Ok(UploadStatus::Committed(committed_bytes(&response))),
        200 | 201 => Ok(UploadStatus::Complete(response.json::<DriveFile>().await?)),
        404 | 410 => Ok(UploadStatus::Expired),
        status => Err(anyhow::anyhow!(
            "unexpected status {status} querying upload status: {}",
            response.text().await.unwrap_or_default()
        )),
    }
}

pub fn committed_bytes(response: &reqwest::Response) -> u64 {
    // `Range: bytes=0-42` means 43 bytes arrived, so the next write starts at 43.
    response
        .headers()
        .get("Range")
        .and_then(|value| value.to_str().ok())
        .and_then(|range| range.rsplit('-').next())
        .and_then(|end| end.parse::<u64>().ok())
        .map(|end| end + 1)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real `files.list` answer, quirks and all: camelCase keys, RFC3339 timestamps,
    /// `size` quoted because the API types it as int64, and folders with no size at all.
    ///
    /// These pin the wire format rather than our structs - every one of these fields was
    /// a place where `rename_all` or the size deserializer could silently stop matching
    /// and turn into "the Drive folder is empty" with nothing in the logs to say why.
    const FILE_LIST: &str = r#"{
        "kind": "drive#fileList",
        "files": [
            {
                "id": "1AbCdEfGhIjK",
                "name": "creative-mc-2024-08-11.tar.gz",
                "size": "734003200",
                "mimeType": "application/gzip",
                "createdTime": "2024-08-11T04:05:06.789Z",
                "modifiedTime": "2024-08-11T04:06:00.000Z"
            },
            {
                "id": "1ZyXwVuTsRqP",
                "name": "Calagopus Backups",
                "mimeType": "application/vnd.google-apps.folder"
            }
        ],
        "nextPageToken": "CAoSJLErMi0"
    }"#;

    #[test]
    fn reads_the_file_list_the_way_google_sends_it() {
        let list: FileList =
            serde_json::from_str(FILE_LIST).expect("google's payload should parse");

        assert_eq!(list.next_page_token.as_deref(), Some("CAoSJLErMi0"));
        assert_eq!(list.files.len(), 2);

        let file = &list.files[0];
        assert_eq!(file.id.as_str(), "1AbCdEfGhIjK");
        assert_eq!(file.name.as_str(), "creative-mc-2024-08-11.tar.gz");
        assert_eq!(file.size, Some(734_003_200));
        assert_eq!(file.mime_type.as_deref(), Some("application/gzip"));
        assert!(file.created_time.is_some(), "createdTime should be parsed");
        assert!(
            file.modified_time.is_some(),
            "modifiedTime should be parsed"
        );

        // A folder has no `size` key at all. `#[serde(default)]` has to absorb that
        // rather than rejecting the whole page, because both live in the same listing.
        let folder = &list.files[1];
        assert_eq!(folder.size, None);
        assert_eq!(folder.created_time, None);
        assert_eq!(
            folder.mime_type.as_deref(),
            Some("application/vnd.google-apps.folder")
        );
    }

    #[test]
    fn a_page_without_a_next_token_has_no_next_page() {
        let list: FileList =
            serde_json::from_str(r#"{"files": []}"#).expect("an empty page should parse");

        assert_eq!(list.next_page_token, None);
        assert!(list.files.is_empty());
    }

    #[test]
    fn a_nameless_file_is_a_decode_error_so_no_projection_may_omit_name() {
        // `find_folder` once asked for `files(id)` alone; Google answered with exactly
        // that, and the decode failed with "missing field `name`" - which surfaced as
        // every push after the first failing at folder resolution. `name` is required
        // in `FileMetadata`, so every `fields` projection in this module has to request
        // both required fields or the answer is an error, not an empty result.
        let nameless = serde_json::from_str::<FileList>(r#"{"files": [{"id": "1"}]}"#);
        assert!(
            nameless.is_err(),
            "an id-only file entry must fail loudly, not parse"
        );
    }

    #[test]
    fn size_accepts_a_bare_number_as_well_as_a_string() {
        // The documented shape is a string, but nothing about a number in its place is
        // worth failing an entire listing over.
        let list: FileList =
            serde_json::from_str(r#"{"files": [{"id": "1", "name": "a", "size": 42}]}"#)
                .expect("a numeric size should parse");

        assert_eq!(list.files[0].size, Some(42));
    }

    #[test]
    fn parents_are_read_from_their_camel_case_key() {
        let file: FileRef =
            serde_json::from_str(r#"{"parents": ["1FolderId"]}"#).expect("should parse");
        assert_eq!(file.parents, Some(vec![CompactString::from("1FolderId")]));

        // `files.get` omits `parents` for anything in the trash, and the caller treats
        // "no parents" as "not ours to touch".
        let missing: FileRef = serde_json::from_str(r#"{}"#).expect("should parse");
        assert_eq!(missing.parents, None);

        // The mime type travels with the parents so the delete route can refuse a
        // folder: trashing one takes every backup inside it.
        let folder: FileRef = serde_json::from_str(
            r#"{"parents": ["1Root"], "mimeType": "application/vnd.google-apps.folder"}"#,
        )
        .expect("should parse");
        assert_eq!(
            folder.mime_type.as_deref(),
            Some("application/vnd.google-apps.folder")
        );
    }

    #[test]
    fn a_size_google_cannot_please_about_is_still_an_error() {
        // Not silently zero: a size we misread would show a wrong byte count on every
        // row of the Files table, and "cannot parse" at least says something happened.
        assert!(
            serde_json::from_str::<FileList>(
                r#"{"files": [{"id": "1", "name": "a", "size": "not-a-number"}]}"#
            )
            .is_err()
        );
    }

    #[test]
    fn app_properties_decode_from_their_camel_case_key() {
        let list: FileList = serde_json::from_str(
            r#"{"files": [{
                "id": "1",
                "name": "a.tar.gz",
                "appProperties": {
                    "dev_caloptreyx_gdrive_server": "09474b34-7c6f-414c-9e49-10970f7df7be",
                    "dev_caloptreyx_gdrive_backup": "75ded8e4-0000-0000-0000-000000000000"
                }
            }]}"#,
        )
        .expect("appProperties should parse");

        let (server_uuid, backup_uuid) = backup_identity(list.files[0].app_properties.as_ref())
            .expect("a tagged file should have an identity");

        assert_eq!(
            server_uuid.to_string(),
            "09474b34-7c6f-414c-9e49-10970f7df7be"
        );
        assert_eq!(
            backup_uuid.to_string(),
            "75ded8e4-0000-0000-0000-000000000000"
        );
    }

    #[test]
    fn a_file_that_is_not_ours_has_no_identity() {
        assert_eq!(backup_identity(None), None);

        // A user-made folder or a hand-dropped file carries no tags, or half of them;
        // neither is an error - it just means "no restore button", not a 500.
        let partial = std::collections::BTreeMap::from([(
            CompactString::from(SERVER_PROPERTY),
            CompactString::from("09474b34-7c6f-414c-9e49-10970f7df7be"),
        )]);
        assert_eq!(backup_identity(Some(&partial)), None);

        let garbage = std::collections::BTreeMap::from([
            (
                CompactString::from(SERVER_PROPERTY),
                CompactString::from("not-a-uuid"),
            ),
            (
                CompactString::from(BACKUP_PROPERTY),
                CompactString::from("75ded8e4-0000-0000-0000-000000000000"),
            ),
        ]);
        assert_eq!(backup_identity(Some(&garbage)), None);
    }

    #[test]
    fn restore_props_decode_when_google_omits_the_optional_keys() {
        // `fields=id,name,...` still means size/trashed/appProperties can be absent -
        // folders have no size, files stamped before this feature have no tags - and
        // DriveFileProps has to absorb that rather than failing the restore outright.
        let props: DriveFileProps =
            serde_json::from_str(r#"{"id": "1", "name": "a.tar.gz", "size": "17740"}"#)
                .expect("id and name alone should parse");

        assert_eq!(props.size, Some(17_740));
        assert_eq!(props.trashed, None);
        assert!(props.app_properties.is_none());
        assert_eq!(backup_identity(props.app_properties.as_ref()), None);
    }
}
