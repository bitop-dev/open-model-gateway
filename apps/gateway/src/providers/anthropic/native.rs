//! A native Messages request's validated envelopes in the body sent: they
//! replace the translated messages, system and tools, and each tool
//! definition takes its schema back from the canonical tool (#872).
use super::*;

/// Native Messages envelopes replace the translated ones.
pub(super) fn set_native_overrides(value: &mut Value, request: &ChatRequest) {
    if let Some(options) = &request.messages_options {
        // Only fully grammar-validated envelopes retained by the Messages
        // normalizer; metadata.user_id was intentionally never retained here.
        if !options.native_messages.is_empty() {
            value["messages"] = json!(options.native_messages);
        }
        if let Some(system) = &options.system {
            value["system"] = system.clone();
        }
        if !options.native_tools.is_empty() {
            // Each definition with its schema back from the canonical tool
            // at its index (`MessagesOptions::native_tools`).
            value["tools"] = Value::Array(
                options
                    .native_tools
                    .iter()
                    .zip(request.tools.iter())
                    .map(|(native, tool)| {
                        let mut definition = native.clone();
                        if let Some(schema) = definition.get_mut("input_schema") {
                            *schema = tool.parameters.clone();
                        }
                        definition
                    })
                    .collect(),
            );
        }
        if let Some(thinking) = &options.thinking {
            value["thinking"] = thinking.clone();
        }
        if let Some(output) = &options.output_config {
            value["output_config"] = output.clone();
        }
        if let Some(context) = &options.context_management {
            value["context_management"] = context.clone();
        }
    }
}
