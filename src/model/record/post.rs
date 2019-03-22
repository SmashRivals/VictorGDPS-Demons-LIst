use super::{Record, RecordStatus};
use crate::{
    citext::CiString,
    config::{EXTENDED_LIST_SIZE, LIST_SIZE},
    error::PointercrateError,
    model::{record::EmbeddedDemon, Demon, EmbeddedPlayer, Submitter},
    operation::{Delete, Get, Post, PostData},
    permissions::PermissionsSet,
    video, Result,
};
use diesel::{Connection, PgConnection, RunQueryDsl};
use log::{debug, info};
use serde_derive::Deserialize;

#[derive(Deserialize, Debug)]
pub struct Submission {
    pub progress: i16,
    pub player: CiString,
    pub demon: CiString,
    #[serde(default)]
    pub video: Option<String>,
    #[serde(default)]
    pub status: RecordStatus,
    #[serde(rename = "check", default)]
    pub verify_only: bool,
}

impl PostData for (Submission, Submitter) {
    fn required_permissions(&self) -> PermissionsSet {
        if self.0.status != RecordStatus::Submitted || self.0.video.is_none() {
            perms!(ListHelper or ListModerator or ListAdministrator)
        } else {
            PermissionsSet::default()
        }
    }
}

impl Post<(Submission, Submitter)> for Option<Record> {
    fn create_from(
        (
            Submission {
                progress,
                player,
                demon,
                video,
                status,
                verify_only,
            },
            submitter,
        ): (Submission, Submitter),
        connection: &PgConnection,
    ) -> Result<Self> {
        info!(
            "Processing record addition '{}% on {} by {} ({})'",
            progress, demon, player, status
        );

        // Banned submitters cannot submit records
        if submitter.banned {
            return Err(PointercrateError::BannedFromSubmissions(submitter.id))?
        }

        // Check if a video exists and validate it
        let video = match video {
            Some(ref video) => Some(video::validate(video)?),
            None => None,
        };

        connection.transaction(||{
            // Resolve player and demon name against the database
            let player = EmbeddedPlayer::get(player.as_ref(), connection)?;
            let demon = Demon::get(demon.as_ref(), connection)?;

            // Banned player can't have records on the list
            if player.banned {
                return Err(PointercrateError::PlayerBanned)
            }

            // Cannot submit records for the legacy list (it is possible to directly add them for list mods)
            if demon.position > *EXTENDED_LIST_SIZE && status == RecordStatus::Submitted {
                return Err(PointercrateError::SubmitLegacy)
            }

            // Can only submit 100% records for the extended list (it is possible to directly add them for list mods)
            if demon.position > *LIST_SIZE && progress != 100 && status == RecordStatus::Submitted {
                return Err(PointercrateError::Non100Extended)
            }

            // Check if the record meets the record requirement for this demon
            if progress > 100 || progress < demon.requirement {
                return Err(PointercrateError::InvalidProgress {
                    requirement: demon.requirement,
                })?
            }

            debug!("Submission is valid, checking for duplicates!");

            // Search for existing records. If no video is provided, we check if a record with the same
            // (demon, player) combination exists. If a video exists, we also check if a record with
            // exactly that video exists. Note that in the second case, two records can be matched,
            // which is why we need the loop here
            let records: Vec<Record> = match video {
                Some(ref video) =>
                    Record::get_existing(player.id, demon.name.as_ref(), video).get_results(connection)?,
                None => Record::by_player_and_demon(player.id, demon.name.as_ref()).get_results(connection)?,
            };

            let video_ref = video.as_ref().map(AsRef::as_ref);
            let mut to_delete = Vec::new();

            for record in records {
                // If we have a duplicate it has one of the three record stats. If its rejected, we
                // reject the submission instantly. If it approved, we reject the submission if the
                // approved record has higher progress than the submission. If its submitted, we do the
                // same, but at the same time, we mark the existing submission with lower progress for
                // deleting.

                // First we reject all records where the video is already in the database
                debug!("Existing record is {} with status {}", record, record.status());

                if record.video == video {
                    return Err(PointercrateError::SubmissionExists {
                        existing: record.id,
                        status: record.status()
                    })
                }

                // Then we reject all records, where the same player/demon combo has already been rejected
                if record.status() == RecordStatus::Rejected {
                    return Err(PointercrateError::SubmissionExists {
                        status: record.status(),
                        existing: record.id,
                    })
                }

                // Then we reject all submissions, where any other record with higher progress with the same player/demon exists (approved or submited)
                if status == RecordStatus::Submitted && record.progress() >= progress {
                    return Err(PointercrateError::SubmissionExists {
                        status: record.status(),
                        existing: record.id,
                    })
                }

                // Lastly, we handle the case where existing and new have the same status. This should be pretty self explaining
                if record.status() == status {
                    if record.progress() < progress {
                        to_delete.push(record)
                    } else {
                        return Err(PointercrateError::SubmissionExists {
                            status: record.status(),
                            existing: record.id,
                        })
                    }
                }

            }

            if verify_only {
                return Ok(None)
            }

            // If none of the duplicates caused us to reject the submission, we now delete submissions marked for deleting
            for record in to_delete {
                debug!(
                    "The submission is duplicated, but new one has higher progress. Deleting old with id {}!",
                    record.id
                );

                record.delete(connection)?;
            }

            debug!("All duplicates either already accepted, or has lower progress, accepting!");

            let id = Record::insert(
                progress,
                video_ref,
                status,
                player.id,
                submitter.id,
                demon.name.as_ref(),
                connection
            )?;

            info!("Submission successful! Created new record with ID {}", id);

            Ok(Some(Record {
                id,
                progress,
                video,
                status,
                player,
                submitter: Some(submitter.id),
                demon: EmbeddedDemon {
                    position: demon.position,
                    name: demon.name
                }
            }))
        })
    }
}
