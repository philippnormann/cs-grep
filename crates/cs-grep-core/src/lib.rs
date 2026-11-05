pub mod abstracts;
pub mod config;
pub mod db;
pub mod dblp;
pub mod output;
pub mod query;

mod error;
mod model;
mod text;

pub use error::{Error, Result};
pub use model::Paper;
