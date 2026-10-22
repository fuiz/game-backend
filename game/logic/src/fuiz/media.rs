//! Media handling for questions (images, videos, etc.)
//!
//! This module defines the media types that can be included in Fuiz questions
//! and answers. Currently, it supports images with plans for future expansion
//! to include videos, audio, and other media types.

use garde::Validate;
use serde::{Deserialize, Serialize};

/// Represents any kind of media content that can be included in questions
///
/// This enum serves as a wrapper for different media types that can be
/// embedded in Fuiz questions or answers. Currently, only images are
/// supported, but this structure allows for future expansion to include
/// videos, audio, and other media formats.
#[derive(Debug, Serialize, Deserialize, Clone, Validate)]
#[garde(context(crate::settings::Settings as ctx))]
pub enum Media {
    /// Image media content
    Image(#[garde(dive)] Image),
}

impl Media {
    /// The words that stand in for the media where it can't be shown, like a
    /// player's recap. Empty when the host wrote none.
    pub fn alt(&self) -> &str {
        match self {
            Self::Image(Image::Corkboard { alt, .. }) => alt,
        }
    }
}

/// Represents different types of image content
///
/// This enum defines the various sources and types of images that can
/// be used in Fuiz questions. Different variants may have different
/// storage mechanisms and validation requirements.
#[derive(Debug, Serialize, Deserialize, Clone, Validate)]
#[garde(context(crate::settings::Settings as ctx))]
pub enum Image {
    /// An image stored in the Corkboard system
    ///
    /// Corkboard is the media storage system used by Fuiz for managing
    /// user-uploaded images. Each image has a unique ID for retrieval
    /// and alt text for accessibility.
    Corkboard {
        /// Unique identifier for the image in the Corkboard system
        #[garde(length(equal = ctx.corkboard.id_length))]
        id: String,
        /// Alternative text for accessibility and display fallbacks
        #[garde(length(max = ctx.corkboard.max_alt_length))]
        alt: String,
    },
}
