pub mod completion;
pub mod embedding;
pub mod model;

pub use completion::*;
pub use embedding::*;
pub use model::OpenAILanguageModel;

// pub const OPENAI_API_URL: &'static str = "https://api.openai.com/v1";
// pub const OPENAI_API_URL: &'static str = "https://openrouter.ai/api/v1";
pub const OPENAI_API_URL: &'static str = "http://127.0.0.1:8080";
