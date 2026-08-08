use crate::model::Sensitivity;

impl Sensitivity {
    /// Stable snake_case label matching the serde representation. Privacy and
    /// egress layers use this instead of re-implementing enum matches.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Internal => "internal",
            Self::Sensitive => "sensitive",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_match_serde_contract() {
        assert_eq!(Sensitivity::Public.as_str(), "public");
        assert_eq!(Sensitivity::Internal.as_str(), "internal");
        assert_eq!(Sensitivity::Sensitive.as_str(), "sensitive");
    }
}
