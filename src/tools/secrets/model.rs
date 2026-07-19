use crate::onepassword::SecretBytes;

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: String) -> Option<Self> {
                (!value.trim().is_empty()).then_some(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }
    };
}

id_type!(AccountId);
id_type!(VaultId);
id_type!(ItemId);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccountSummary {
    pub id: AccountId,
    pub label: String,
    pub selectors: Vec<String>,
}

impl AccountSummary {
    pub fn matches(&self, selector: &str) -> bool {
        self.id.as_str() == selector || self.selectors.iter().any(|value| value == selector)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VaultSummary {
    pub id: VaultId,
    pub name: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ItemRef {
    pub account_id: AccountId,
    pub vault_id: VaultId,
    pub item_id: ItemId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ItemSummary {
    pub reference: ItemRef,
    pub title: String,
    pub vault_name: String,
    pub category: String,
    pub tags: Vec<String>,
    pub urls: Vec<String>,
    pub additional_information: Option<String>,
}

impl ItemSummary {
    pub fn is_login(&self) -> bool {
        self.category == "LOGIN"
    }

    pub fn search_text(&self) -> String {
        let mut parts = vec![self.title.as_str(), self.vault_name.as_str(), self.category.as_str()];
        parts.extend(self.tags.iter().map(String::as_str));
        parts.extend(self.urls.iter().map(String::as_str));
        if let Some(additional) = self.additional_information.as_deref() {
            parts.push(additional);
        }
        parts.join(" ")
    }
}

pub struct CreateLoginRequest {
    pub account_id: AccountId,
    pub vault_id: VaultId,
    pub title: String,
    pub username: String,
    pub url: String,
    pub password: Option<SecretBytes>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PasswordRecipe {
    pub length: u8,
}

impl Default for PasswordRecipe {
    fn default() -> Self {
        Self { length: 32 }
    }
}

impl PasswordRecipe {
    pub fn as_argument(self) -> String {
        format!("letters,digits,symbols,{}", self.length)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item() -> ItemSummary {
        ItemSummary {
            reference: ItemRef {
                account_id: AccountId::new("account".to_owned()).unwrap(),
                vault_id: VaultId::new("vault".to_owned()).unwrap(),
                item_id: ItemId::new("item".to_owned()).unwrap(),
            },
            title: "Production Login".to_owned(),
            vault_name: "Engineering".to_owned(),
            category: "LOGIN".to_owned(),
            tags: vec!["infra".to_owned()],
            urls: vec!["https://example.test".to_owned()],
            additional_information: Some("admin".to_owned()),
        }
    }

    #[test]
    fn metadata_search_text_contains_only_summary_fields() {
        let text = item().search_text();
        assert!(text.contains("Production Login"));
        assert!(text.contains("Engineering"));
        assert!(text.contains("infra"));
        assert!(text.contains("example.test"));
    }

    #[test]
    fn account_matches_only_documented_selectors() {
        let account = AccountSummary {
            id: AccountId::new("id".to_owned()).unwrap(),
            label: "Personal".to_owned(),
            selectors: vec!["my".to_owned(), "my.1password.com".to_owned()],
        };
        assert!(account.matches("id"));
        assert!(account.matches("my"));
        assert!(!account.matches("Personal"));
    }
}
