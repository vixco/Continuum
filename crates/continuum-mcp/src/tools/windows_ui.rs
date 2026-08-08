//! # Focused-element Windows UI Automation bridge
//!
//! The bridge deliberately exposes no coordinate input and no arbitrary tree
//! search. Mutations apply only to the element that has focus at execution.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Deserialize, Serialize, JsonSchema)]
pub struct WindowsUiEmptyRequest {}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct WindowsUiSetValueRequest {
    /// Bounded text; redacted by the common audit sanitizer.
    pub content: String,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct WindowsUiElementResponse {
    pub name: String,
    pub automation_id: String,
    pub control_type_id: i32,
    pub is_password: bool,
}

#[cfg(windows)]
fn focused() -> Result<
    (
        windows::Win32::UI::Accessibility::IUIAutomationElement,
        WindowsUiElementResponse,
    ),
    WindowsUiError,
> {
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
    };
    use windows::Win32::UI::Accessibility::{CUIAutomation, IUIAutomation};
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED)
            .ok()
            .map_err(|e| WindowsUiError::Automation(e.to_string()))?;
        let automation: IUIAutomation =
            CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| WindowsUiError::Automation(e.to_string()))?;
        let element = automation
            .GetFocusedElement()
            .map_err(|e| WindowsUiError::Automation(e.to_string()))?;
        let is_password = element
            .CurrentIsPassword()
            .map_err(|e| WindowsUiError::Automation(e.to_string()))?
            .as_bool();
        let response = WindowsUiElementResponse {
            name: if is_password {
                "[PASSWORD FIELD]".into()
            } else {
                element
                    .CurrentName()
                    .map_err(|e| WindowsUiError::Automation(e.to_string()))?
                    .to_string()
            },
            automation_id: element
                .CurrentAutomationId()
                .map_err(|e| WindowsUiError::Automation(e.to_string()))?
                .to_string(),
            control_type_id: element
                .CurrentControlType()
                .map_err(|e| WindowsUiError::Automation(e.to_string()))?
                .0,
            is_password,
        };
        Ok((element, response))
    }
}

pub fn focused_element() -> Result<WindowsUiElementResponse, WindowsUiError> {
    #[cfg(windows)]
    {
        focused().map(|(_, response)| response)
    }
    #[cfg(not(windows))]
    {
        Err(WindowsUiError::Unsupported)
    }
}

pub fn invoke_focused() -> Result<WindowsUiElementResponse, WindowsUiError> {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::UI::Accessibility::{IUIAutomationInvokePattern, UIA_InvokePatternId};
        let (element, response) = focused()?;
        if response.is_password {
            return Err(WindowsUiError::Password);
        }
        let pattern: IUIAutomationInvokePattern = element
            .GetCurrentPatternAs(UIA_InvokePatternId)
            .map_err(|_| WindowsUiError::PatternUnavailable)?;
        pattern
            .Invoke()
            .map_err(|e| WindowsUiError::Automation(e.to_string()))?;
        Ok(response)
    }
    #[cfg(not(windows))]
    {
        Err(WindowsUiError::Unsupported)
    }
}

pub fn set_focused_value(
    request: &WindowsUiSetValueRequest,
) -> Result<WindowsUiElementResponse, WindowsUiError> {
    if request.content.len() > 64 * 1024 {
        return Err(WindowsUiError::TooLarge);
    }
    #[cfg(windows)]
    unsafe {
        use windows::core::BSTR;
        use windows::Win32::UI::Accessibility::{IUIAutomationValuePattern, UIA_ValuePatternId};
        let (element, response) = focused()?;
        if response.is_password {
            return Err(WindowsUiError::Password);
        }
        let pattern: IUIAutomationValuePattern = element
            .GetCurrentPatternAs(UIA_ValuePatternId)
            .map_err(|_| WindowsUiError::PatternUnavailable)?;
        if pattern
            .CurrentIsReadOnly()
            .map_err(|e| WindowsUiError::Automation(e.to_string()))?
            .as_bool()
        {
            return Err(WindowsUiError::ReadOnly);
        }
        pattern
            .SetValue(&BSTR::from(&request.content))
            .map_err(|e| WindowsUiError::Automation(e.to_string()))?;
        Ok(response)
    }
    #[cfg(not(windows))]
    {
        Err(WindowsUiError::Unsupported)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WindowsUiError {
    #[error("Windows UI Automation is unsupported on this platform")]
    Unsupported,
    #[error("UI Automation failed: {0}")]
    Automation(String),
    #[error("focused element does not support the requested pattern")]
    PatternUnavailable,
    #[error("password fields are always blocked")]
    Password,
    #[error("focused value is read-only")]
    ReadOnly,
    #[error("content exceeds 64 KiB")]
    TooLarge,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_oversized_values_before_automation() {
        let request = WindowsUiSetValueRequest {
            content: "x".repeat(65 * 1024),
        };
        assert!(matches!(
            set_focused_value(&request),
            Err(WindowsUiError::TooLarge)
        ));
    }
}
