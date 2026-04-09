use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Notification {
    Email { address: String, subject: String, message: String },
    Sms { phone_number: String, message: String },
    Web { address: String, data: String, headers: std::collections::HashMap<String, String> },
}
