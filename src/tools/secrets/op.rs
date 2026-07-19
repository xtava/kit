use std::io::{self, Write};

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::onepassword::{
    OpClient as SharedOpClient, OpError, SecretBytes, SecretReference, SensitiveBuffer,
    StderrPolicy,
};

use super::model::{
    AccountId, AccountSummary, CreateLoginRequest, ItemId, ItemRef, ItemSummary, PasswordRecipe,
    VaultId, VaultSummary,
};

const MAX_CREATE_JSON_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoginField {
    Username,
    Password,
}

impl LoginField {
    fn reference_name(self) -> &'static str {
        match self {
            Self::Username => "username",
            Self::Password => "password",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Username => "Username",
            Self::Password => "Password",
        }
    }
}

#[derive(Clone)]
pub struct OpClient {
    shared: SharedOpClient,
}

impl OpClient {
    pub fn new() -> Self {
        Self { shared: SharedOpClient::new() }
    }

    #[cfg(test)]
    pub(crate) fn with_executable(executable: std::path::PathBuf) -> Self {
        Self { shared: SharedOpClient::with_executable(executable) }
    }

    pub async fn version(&self) -> Result<(), OpError> {
        self.shared.version().await
    }

    pub async fn accounts(&self) -> Result<Vec<AccountSummary>, OpError> {
        let raw: Vec<RawAccount> =
            self.shared.json("list accounts", ["account", "list", "--format=json"]).await?;
        raw.into_iter().map(AccountSummary::try_from).collect()
    }

    pub async fn vaults(&self, account: &AccountId) -> Result<Vec<VaultSummary>, OpError> {
        let raw: Vec<RawVault> = self
            .shared
            .json("list vaults", ["vault", "list", "--format=json", "--account", account.as_str()])
            .await?;
        raw.into_iter().map(VaultSummary::try_from).collect()
    }

    pub async fn items(&self, account: &AccountId) -> Result<Vec<ItemSummary>, OpError> {
        let raw: Vec<RawItem> = self
            .shared
            .json("list items", ["item", "list", "--format=json", "--account", account.as_str()])
            .await?;
        raw.into_iter().map(|item| item.into_summary(account.clone())).collect()
    }

    pub async fn field(
        &self,
        reference: &ItemRef,
        field: LoginField,
    ) -> Result<SecretBytes, OpError> {
        let secret_reference = secret_reference(reference, field)?;
        self.shared
            .read_reference_for_account(&secret_reference, reference.account_id.as_str())
            .await
    }

    pub async fn create_login(&self, mut request: CreateLoginRequest) -> Result<(), OpError> {
        let body = create_body(&request)?;
        let args = create_args(&request);
        // The fixed JSON stdin buffer is now the sole Kit-owned copy needed by the subprocess.
        request.password = None;
        self.shared.status_with_stdin("create login", args, body).await
    }

    pub async fn rotate_password(&self, reference: &ItemRef) -> Result<(), OpError> {
        let args = vec![
            "item".to_owned(),
            "edit".to_owned(),
            reference.item_id.as_str().to_owned(),
            "--vault".to_owned(),
            reference.vault_id.as_str().to_owned(),
            format!("--generate-password={}", PasswordRecipe::default().as_argument()),
            "--format=json".to_owned(),
            "--account".to_owned(),
            reference.account_id.as_str().to_owned(),
        ];
        self.shared.status("rotate password", &args, StderrPolicy::Discard).await
    }

    pub async fn archive(&self, reference: &ItemRef) -> Result<(), OpError> {
        let args = vec![
            "item".to_owned(),
            "delete".to_owned(),
            reference.item_id.as_str().to_owned(),
            "--vault".to_owned(),
            reference.vault_id.as_str().to_owned(),
            "--archive".to_owned(),
            "--account".to_owned(),
            reference.account_id.as_str().to_owned(),
        ];
        self.shared.status("archive item", &args, StderrPolicy::CaptureSanitized).await
    }
}

impl Default for OpClient {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Deserialize)]
struct RawAccount {
    #[serde(default, alias = "account_uuid")]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    shorthand: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    email: String,
}

impl TryFrom<RawAccount> for AccountSummary {
    type Error = OpError;

    fn try_from(raw: RawAccount) -> Result<Self, Self::Error> {
        let id = AccountId::new(raw.id).ok_or(OpError::InvalidResponse {
            operation: "list accounts",
            reason: "account omitted its id",
        })?;
        let label = [raw.name.as_str(), raw.shorthand.as_str(), raw.url.as_str()]
            .into_iter()
            .find(|value| !value.is_empty())
            .unwrap_or("1Password account")
            .to_owned();
        let selectors = [raw.shorthand, raw.url, raw.email]
            .into_iter()
            .filter(|value| !value.is_empty())
            .collect();
        Ok(Self { id, label, selectors })
    }
}

#[derive(Deserialize)]
struct RawVault {
    #[serde(default)]
    id: String,
    #[serde(default, alias = "title")]
    name: String,
}

impl TryFrom<RawVault> for VaultSummary {
    type Error = OpError;

    fn try_from(raw: RawVault) -> Result<Self, Self::Error> {
        let id = VaultId::new(raw.id).ok_or(OpError::InvalidResponse {
            operation: "list vaults",
            reason: "vault omitted its id",
        })?;
        Ok(Self { id, name: raw.name })
    }
}

#[derive(Deserialize)]
struct RawItem {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    category: String,
    #[serde(default)]
    vault: Option<RawVault>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    urls: Vec<RawUrl>,
    #[serde(default)]
    additional_information: Option<String>,
}

impl RawItem {
    fn into_summary(self, account_id: AccountId) -> Result<ItemSummary, OpError> {
        let item_id = ItemId::new(self.id).ok_or(OpError::InvalidResponse {
            operation: "list items",
            reason: "item omitted its id",
        })?;
        let vault = self.vault.ok_or(OpError::InvalidResponse {
            operation: "list items",
            reason: "item omitted its vault",
        })?;
        let vault_id = VaultId::new(vault.id).ok_or(OpError::InvalidResponse {
            operation: "list items",
            reason: "item vault omitted its id",
        })?;
        Ok(ItemSummary {
            reference: ItemRef { account_id, vault_id, item_id },
            title: self.title,
            vault_name: vault.name,
            category: self.category,
            tags: self.tags,
            urls: self.urls.into_iter().map(|url| url.href).filter(|url| !url.is_empty()).collect(),
            additional_information: self.additional_information,
        })
    }
}

#[derive(Deserialize)]
struct RawUrl {
    #[serde(default)]
    href: String,
}

#[derive(Serialize)]
struct LoginTemplate<'a> {
    title: &'a str,
    category: &'static str,
    fields: Vec<TemplateField<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    urls: Vec<TemplateUrl<'a>>,
}

#[derive(Serialize)]
struct TemplateField<'a> {
    id: &'static str,
    #[serde(rename = "type")]
    kind: &'static str,
    purpose: &'static str,
    label: &'static str,
    value: &'a str,
}

#[derive(Serialize)]
struct TemplateUrl<'a> {
    primary: bool,
    href: &'a str,
}

fn create_body(request: &CreateLoginRequest) -> Result<Zeroizing<Vec<u8>>, OpError> {
    let mut fields = Vec::new();
    if !request.username.is_empty() {
        fields.push(TemplateField {
            id: "username",
            kind: "STRING",
            purpose: "USERNAME",
            label: "username",
            value: &request.username,
        });
    }
    if let Some(password) = request.password.as_ref() {
        fields.push(TemplateField {
            id: "password",
            kind: "CONCEALED",
            purpose: "PASSWORD",
            label: "password",
            value: password.as_str(),
        });
    }
    let urls = (!request.url.is_empty())
        .then_some(TemplateUrl { primary: true, href: &request.url })
        .into_iter()
        .collect();
    let template = LoginTemplate { title: &request.title, category: "LOGIN", fields, urls };
    let serialized_len = serialized_len(&template)?;
    let mut writer = SensitiveBuffer::new(serialized_len);
    serde_json::to_writer(&mut writer, &template)
        .map_err(|source| OpError::InvalidJson { operation: "serialize login", source })?;
    Ok(writer.into_bytes())
}

fn create_args(request: &CreateLoginRequest) -> Vec<String> {
    let mut args = vec![
        "item".to_owned(),
        "create".to_owned(),
        "-".to_owned(),
        "--vault".to_owned(),
        request.vault_id.as_str().to_owned(),
        "--account".to_owned(),
        request.account_id.as_str().to_owned(),
    ];
    if request.password.is_none() {
        args.push(format!("--generate-password={}", PasswordRecipe::default().as_argument()));
    }
    args
}

struct BoundedCounter {
    length: usize,
    limit: usize,
    exceeded: bool,
}

impl BoundedCounter {
    fn new(limit: usize) -> Self {
        Self { length: 0, limit, exceeded: false }
    }
}

impl Write for BoundedCounter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let Some(length) = self.length.checked_add(bytes.len()) else {
            self.exceeded = true;
            return Err(io::Error::other("serialized login exceeded its size limit"));
        };
        if length > self.limit {
            self.exceeded = true;
            return Err(io::Error::other("serialized login exceeded its size limit"));
        }
        self.length = length;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn serialized_len(template: &LoginTemplate<'_>) -> Result<usize, OpError> {
    let mut counter = BoundedCounter::new(MAX_CREATE_JSON_BYTES);
    let result = serde_json::to_writer(&mut counter, template);
    if counter.exceeded {
        return Err(OpError::RequestTooLarge {
            operation: "serialize login",
            limit: MAX_CREATE_JSON_BYTES,
        });
    }
    result.map_err(|source| OpError::InvalidJson { operation: "serialize login", source })?;
    Ok(counter.length)
}

fn secret_reference(reference: &ItemRef, field: LoginField) -> Result<SecretReference, OpError> {
    SecretReference::new(format!(
        "op://{}/{}/{}",
        reference.vault_id.as_str(),
        reference.item_id.as_str(),
        field.reference_name()
    ))
    .map_err(|_| OpError::InvalidResponse {
        operation: "read login field",
        reason: "item identifiers could not form a valid 1Password reference",
    })
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, process::Stdio};

    use crate::onepassword::MAX_SECRET_BYTES;

    use super::*;

    #[cfg(unix)]
    struct FakeOp {
        directory: PathBuf,
        executable: PathBuf,
    }

    #[cfg(unix)]
    impl FakeOp {
        fn new(script: &str) -> Self {
            use std::io::Write as _;
            use std::os::unix::fs::PermissionsExt;
            use std::sync::atomic::{AtomicU64, Ordering};

            static NEXT_ID: AtomicU64 = AtomicU64::new(0);
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let directory = std::env::temp_dir()
                .join(format!("kit-secrets-op-test-{}-{id}", std::process::id()));
            std::fs::create_dir(&directory).unwrap();
            let executable = directory.join("op");
            // Keep the executable's write-open descriptor out of the concurrent test process so
            // another fork cannot inherit it and make the subsequent exec fail with ETXTBSY.
            let mut writer = std::process::Command::new("/bin/sh")
                .args(["-c", "umask 077; /bin/cat > \"$1\"", "kit-fake-op-writer"])
                .arg(&executable)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let mut stdin = writer.stdin.take().unwrap();
            stdin.write_all(script.as_bytes()).unwrap();
            drop(stdin);
            assert!(writer.wait().unwrap().success());
            let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
            permissions.set_mode(0o700);
            std::fs::set_permissions(&executable, permissions).unwrap();
            Self { directory, executable }
        }

        fn client(&self) -> OpClient {
            OpClient::with_executable(self.executable.clone())
        }
    }

    #[cfg(unix)]
    impl Drop for FakeOp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    fn account() -> AccountId {
        AccountId::new("account-id".to_owned()).unwrap()
    }

    fn vault() -> VaultId {
        VaultId::new("vault-id".to_owned()).unwrap()
    }

    fn item_reference() -> ItemRef {
        ItemRef {
            account_id: account(),
            vault_id: vault(),
            item_id: ItemId::new("item-id".to_owned()).unwrap(),
        }
    }

    fn synthetic_secret() -> SecretBytes {
        SecretBytes::from_utf8(b"synthetic-sentinel".to_vec()).unwrap()
    }

    #[test]
    fn parses_item_summaries_without_secret_fields() {
        let raw: Vec<RawItem> = serde_json::from_str(
            r#"[{"id":"item-id","title":"Example","category":"LOGIN","vault":{"id":"vault-id","name":"Private"},"tags":["work"],"urls":[{"href":"https://example.test"}]}]"#,
        )
        .unwrap();
        let item = raw.into_iter().next().unwrap().into_summary(account()).unwrap();
        assert_eq!(item.title, "Example");
        assert_eq!(item.vault_name, "Private");
        assert_eq!(item.urls, vec!["https://example.test"]);
    }

    #[test]
    fn create_places_manual_password_only_in_fixed_stdin_json() {
        let request = CreateLoginRequest {
            account_id: account(),
            vault_id: vault(),
            title: "Example".to_owned(),
            username: "user".to_owned(),
            url: "https://example.test".to_owned(),
            password: Some(synthetic_secret()),
        };
        let args = create_args(&request);
        let body = create_body(&request).unwrap();

        assert!(args.iter().all(|argument| !argument.contains("synthetic-sentinel")));
        assert!(String::from_utf8_lossy(&body).contains("synthetic-sentinel"));
        assert!(body.len() <= MAX_CREATE_JSON_BYTES);
    }

    #[test]
    fn field_reference_uses_ids_and_a_fixed_built_in_name() {
        let reference = item_reference();
        assert_eq!(
            secret_reference(&reference, LoginField::Password).unwrap().as_str(),
            "op://vault-id/item-id/password"
        );
        assert_eq!(
            secret_reference(&reference, LoginField::Username).unwrap().as_str(),
            "op://vault-id/item-id/username"
        );
    }

    #[test]
    fn serialized_size_counter_uses_an_explicit_limit() {
        let mut counter = BoundedCounter::new(3);
        counter.write_all(b"abc").unwrap();
        assert!(counter.write_all(b"d").is_err());
        assert!(counter.exceeded);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fake_op_proves_field_args_and_bounded_raw_stdout_contract() {
        let fake = FakeOp::new(
            r#"#!/bin/sh
[ "$#" -eq 6 ] || exit 90
[ "$1" = read ] || exit 91
[ "$2" = op://vault-id/item-id/password ] || exit 92
[ "$3" = --account ] && [ "$4" = account-id ] || exit 93
[ "$5" = --no-newline ] && [ "$6" = --no-color ] || exit 94
printf %s synthetic-read-value
"#,
        );

        let value = fake.client().field(&item_reference(), LoginField::Password).await.unwrap();
        assert_eq!(value.as_str(), "synthetic-read-value");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fake_op_proves_manual_create_secret_is_stdin_only() {
        let fake = FakeOp::new(
            r#"#!/bin/sh
[ "$1" = item ] && [ "$2" = create ] && [ "$3" = - ] || exit 90
for argument in "$@"; do
  case "$argument" in
    *synthetic-sentinel*|*Example*|*example.test*|*user*) exit 91 ;;
  esac
done
body=$(cat)
case "$body" in
  *synthetic-sentinel*) ;;
  *) exit 92 ;;
esac
printf %s 'response-output-must-not-be-read'
"#,
        );
        let request = CreateLoginRequest {
            account_id: account(),
            vault_id: vault(),
            title: "Example".to_owned(),
            username: "user".to_owned(),
            url: "https://example.test".to_owned(),
            password: Some(synthetic_secret()),
        };

        fake.client().create_login(request).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn fake_op_output_over_limit_is_rejected() {
        let fake = FakeOp::new(
            r#"#!/bin/sh
i=0
while [ "$i" -lt 4097 ]; do
  printf x
  i=$((i + 1))
done
"#,
        );

        let result = fake.client().field(&item_reference(), LoginField::Password).await;
        let Err(error) = result else { panic!("oversized secret output unexpectedly succeeded") };
        assert!(matches!(error, OpError::ResponseTooLarge { limit: MAX_SECRET_BYTES, .. }));
    }
}
