//! `ListAvailableProfiles` 响应模型。

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ListAvailableProfilesResponse {
    #[serde(default)]
    pub profiles: Vec<AvailableProfile>,
    #[serde(default)]
    #[allow(dead_code)]
    pub next_token: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AvailableProfile {
    #[serde(default)]
    pub arn: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    pub profile_name: Option<String>,
}

impl ListAvailableProfilesResponse {
    pub fn first_arn(&self) -> Option<&str> {
        self.profiles
            .iter()
            .filter_map(|profile| profile.arn.as_deref())
            .find(|arn| !arn.trim().is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::ListAvailableProfilesResponse;

    #[test]
    fn first_arn_skips_blank_profiles() {
        let response: ListAvailableProfilesResponse = serde_json::from_str(
            r#"{"profiles":[{"arn":""},{"arn":"arn:aws:codewhisperer:us-east-1:123:profile/REAL"}]}"#,
        )
        .unwrap();

        assert_eq!(
            response.first_arn(),
            Some("arn:aws:codewhisperer:us-east-1:123:profile/REAL")
        );
    }
}
