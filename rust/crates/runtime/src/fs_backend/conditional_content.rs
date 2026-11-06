//! Adapt the kernel's paired content outcomes to the filesystem boundary.

use std::io;

use kernel::kernel::conditional_content::{
    ConditionalContentError, ConditionalContentResult, VersionedContent,
};

use super::{kernel_err, VersionToken, WriteOutcome};

pub(super) fn read(
    result: Result<Option<VersionedContent>, ConditionalContentError>,
) -> io::Result<Option<(Vec<u8>, VersionToken)>> {
    result
        .map(|snapshot| snapshot.map(|image| (image.content, VersionToken(image.version))))
        .map_err(error)
}

pub(super) fn write(
    result: Result<ConditionalContentResult, ConditionalContentError>,
) -> io::Result<WriteOutcome> {
    match result {
        Ok(ConditionalContentResult::Committed { version }) => {
            Ok(WriteOutcome::Committed(VersionToken(version)))
        }
        Ok(ConditionalContentResult::Conflict { current_version }) => Ok(WriteOutcome::Conflict {
            current: current_version.map(VersionToken),
        }),
        Err(ConditionalContentError::Unknown(_)) => Ok(WriteOutcome::Unknown),
        Err(failure) => Err(error(failure)),
    }
}

fn error(error: ConditionalContentError) -> io::Error {
    match error {
        ConditionalContentError::Unsupported => io::Error::new(io::ErrorKind::Unsupported, error),
        ConditionalContentError::Failed(error) => kernel_err(error),
        ConditionalContentError::Unknown(_) => io::Error::other(error),
    }
}
