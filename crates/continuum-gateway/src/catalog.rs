//! Static provider catalog. Presets only prefill the Add Provider form —
//! adding an entry here is a data change, not a code change.

use crate::types::ProviderKind;

#[derive(Debug, Clone, Copy)]
pub struct CatalogEntry {
    pub id: &'static str,
    pub label: &'static str,
    pub kind: ProviderKind,
    pub default_base_url: Option<&'static str>,
    pub needs_key: bool,
    /// Shown as placeholder text in the key field, e.g. the provider's key-name convention.
    pub key_hint: &'static str,
    pub docs_url: &'static str,
}

const C: &[CatalogEntry] = &[
    CatalogEntry {
        id: "lmstudio",
        label: "LM Studio",
        kind: ProviderKind::OpenAiCompat,
        default_base_url: Some("http://localhost:1234/v1"),
        needs_key: false,
        key_hint: "",
        docs_url: "https://lmstudio.ai/docs",
    },
    CatalogEntry {
        id: "ollama",
        label: "Ollama",
        kind: ProviderKind::OpenAiCompat,
        default_base_url: Some("http://localhost:11434/v1"),
        needs_key: false,
        key_hint: "",
        docs_url: "https://ollama.com",
    },
    CatalogEntry {
        id: "claude-cli",
        label: "Claude Code (subscription)",
        kind: ProviderKind::ClaudeCli,
        default_base_url: None,
        needs_key: false,
        key_hint: "",
        docs_url: "https://code.claude.com/docs",
    },
    CatalogEntry {
        id: "anthropic",
        label: "Anthropic API",
        kind: ProviderKind::Anthropic,
        default_base_url: Some("https://api.anthropic.com"),
        needs_key: true,
        key_hint: "sk-ant-…",
        docs_url: "https://platform.claude.com/docs",
    },
    CatalogEntry {
        id: "openai",
        label: "OpenAI",
        kind: ProviderKind::OpenAiCompat,
        default_base_url: Some("https://api.openai.com/v1"),
        needs_key: true,
        key_hint: "OPENAI_API_KEY",
        docs_url: "https://platform.openai.com/docs",
    },
    CatalogEntry {
        id: "openrouter",
        label: "OpenRouter",
        kind: ProviderKind::OpenAiCompat,
        default_base_url: Some("https://openrouter.ai/api/v1"),
        needs_key: true,
        key_hint: "OPENROUTER_API_KEY",
        docs_url: "https://openrouter.ai/docs",
    },
    CatalogEntry {
        id: "deepseek",
        label: "DeepSeek",
        kind: ProviderKind::OpenAiCompat,
        default_base_url: Some("https://api.deepseek.com/v1"),
        needs_key: true,
        key_hint: "DEEPSEEK_API_KEY",
        docs_url: "https://api-docs.deepseek.com",
    },
    CatalogEntry {
        id: "fireworks",
        label: "Fireworks AI",
        kind: ProviderKind::OpenAiCompat,
        default_base_url: Some("https://api.fireworks.ai/inference/v1"),
        needs_key: true,
        key_hint: "FIREWORKS_API_KEY",
        docs_url: "https://docs.fireworks.ai",
    },
    CatalogEntry {
        id: "kimi",
        label: "Kimi / Moonshot",
        kind: ProviderKind::OpenAiCompat,
        default_base_url: Some("https://api.moonshot.ai/v1"),
        needs_key: true,
        key_hint: "KIMI_API_KEY",
        docs_url: "https://platform.moonshot.ai",
    },
    CatalogEntry {
        id: "kimi-cn",
        label: "Kimi / Moonshot (China)",
        kind: ProviderKind::OpenAiCompat,
        default_base_url: Some("https://api.moonshot.cn/v1"),
        needs_key: true,
        key_hint: "KIMI_CN_API_KEY",
        docs_url: "https://platform.moonshot.cn",
    },
    CatalogEntry {
        id: "zai",
        label: "z.ai / GLM",
        kind: ProviderKind::OpenAiCompat,
        default_base_url: Some("https://api.z.ai/api/paas/v4"),
        needs_key: true,
        key_hint: "GLM_API_KEY",
        docs_url: "https://docs.z.ai",
    },
    CatalogEntry {
        id: "minimax",
        label: "MiniMax",
        kind: ProviderKind::OpenAiCompat,
        default_base_url: Some("https://api.minimax.io/v1"),
        needs_key: true,
        key_hint: "MINIMAX_API_KEY",
        docs_url: "https://platform.minimax.io",
    },
    CatalogEntry {
        id: "xai",
        label: "xAI (Grok)",
        kind: ProviderKind::OpenAiCompat,
        default_base_url: Some("https://api.x.ai/v1"),
        needs_key: true,
        key_hint: "XAI_API_KEY",
        docs_url: "https://docs.x.ai",
    },
    CatalogEntry {
        id: "stepfun",
        label: "StepFun",
        kind: ProviderKind::OpenAiCompat,
        default_base_url: Some("https://api.stepfun.com/v1"),
        needs_key: true,
        key_hint: "STEPFUN_API_KEY",
        docs_url: "https://platform.stepfun.com",
    },
    CatalogEntry {
        id: "nvidia",
        label: "NVIDIA Build",
        kind: ProviderKind::OpenAiCompat,
        default_base_url: Some("https://integrate.api.nvidia.com/v1"),
        needs_key: true,
        key_hint: "NVIDIA_API_KEY",
        docs_url: "https://build.nvidia.com",
    },
    CatalogEntry {
        id: "huggingface",
        label: "Hugging Face",
        kind: ProviderKind::OpenAiCompat,
        default_base_url: Some("https://router.huggingface.co/v1"),
        needs_key: true,
        key_hint: "HF_TOKEN",
        docs_url: "https://huggingface.co/docs",
    },
    CatalogEntry {
        id: "gemini",
        label: "Google Gemini",
        kind: ProviderKind::OpenAiCompat,
        default_base_url: Some("https://generativelanguage.googleapis.com/v1beta/openai"),
        needs_key: true,
        key_hint: "GEMINI_API_KEY",
        docs_url: "https://ai.google.dev",
    },
    CatalogEntry {
        id: "dashscope",
        label: "Qwen (DashScope)",
        kind: ProviderKind::OpenAiCompat,
        default_base_url: Some("https://dashscope-intl.aliyuncs.com/compatible-mode/v1"),
        needs_key: true,
        key_hint: "DASHSCOPE_API_KEY",
        docs_url: "https://www.alibabacloud.com/help/en/model-studio",
    },
    CatalogEntry {
        id: "custom",
        label: "Custom endpoint",
        kind: ProviderKind::OpenAiCompat,
        default_base_url: None,
        needs_key: false,
        key_hint: "optional",
        docs_url: "",
    },
];

pub fn catalog() -> &'static [CatalogEntry] {
    C
}

pub fn find(id: &str) -> Option<&'static CatalogEntry> {
    C.iter().find(|e| e.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_nonempty() {
        let mut seen = std::collections::HashSet::new();
        for e in catalog() {
            assert!(!e.id.is_empty() && !e.label.is_empty());
            assert!(seen.insert(e.id), "duplicate catalog id {}", e.id);
        }
        assert!(catalog().len() >= 18);
    }

    #[test]
    fn base_urls_parse_and_locals_are_keyless() {
        for e in catalog() {
            if let Some(u) = e.default_base_url {
                url::Url::parse(u).unwrap_or_else(|_| panic!("bad url for {}: {u}", e.id));
            }
        }
        for id in ["lmstudio", "ollama", "claude-cli"] {
            assert!(!find(id).expect(id).needs_key, "{id} must be keyless");
        }
        assert!(find("custom").expect("custom").default_base_url.is_none());
        assert_eq!(
            find("anthropic").expect("anthropic").kind,
            crate::types::ProviderKind::Anthropic
        );
        assert_eq!(
            find("claude-cli").expect("claude-cli").kind,
            crate::types::ProviderKind::ClaudeCli
        );
    }
}
