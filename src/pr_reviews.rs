use std::collections::{HashMap, HashSet};

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use octocrab::models::{
    AuthorAssociation, UserId,
    pulls::{Review, ReviewState},
    webhook_events::{WebhookEvent, WebhookEventPayload},
};

use crate::{MOFF_ORG, MOFF_REPO, octo_ext::get_requested_reviewers};

pub async fn on_pull_request_review(event: WebhookEvent) -> Result<()> {
    let pr = (match event.specific {
        WebhookEventPayload::PullRequestReview(payload) => *payload,
        _ => bail!("Unexpected webhook payload type"),
    })
    .pull_request
    .number;

    let octo = octocrab::instance();

    let requested_reviewers = get_requested_reviewers(&octo, MOFF_ORG, MOFF_REPO, pr)
        .await?
        .users
        .iter()
        .map(|it| it.id)
        .collect::<HashSet<_>>();
    let reviews = octo
        .pulls(MOFF_ORG, MOFF_REPO)
        .list_reviews(pr)
        // We only get 100 because we only care about the most recent ones AND how often are there more than 100 reviews on one PR?
        .per_page(100)
        .send()
        .await?;
    let mut latest_reviews_by_member = get_latest_reviews_by_user(reviews.items);

    latest_reviews_by_member.retain(|it, _| !requested_reviewers.contains(it));
    let mut state = MoffLabels::AwaitingReview;
    for (status, _) in latest_reviews_by_member.values() {
        match status {
            ReviewState::ChangesRequested => {
                state = MoffLabels::ChangesRequested;
                break;
            }
            ReviewState::Approved => {
                state = MoffLabels::Approved;
            }
            s => unreachable!("We should have filtered out {:?} earlier", s),
        }
    }

    let current_labels = octo
        .issues(MOFF_ORG, MOFF_REPO)
        .list_labels_for_issue(pr)
        .send()
        .await?;
    let current_labels = current_labels
        .items
        .iter()
        .filter_map(|it| MoffLabels::try_from(&it.name[..]).ok());

    let mut has_desired_label = false;
    for current_label in current_labels {
        if current_label == state {
            has_desired_label = true;
        } else {
            octo.issues(MOFF_ORG, MOFF_REPO)
                .remove_label(pr, current_label.to_label_string())
                .await?;
        }
    }

    if !has_desired_label {
        octo.issues(MOFF_ORG, MOFF_REPO)
            .add_labels(pr, &[state.to_label_string().to_string()])
            .await?;
    }

    Ok(())
}

fn get_latest_reviews_by_user(
    reviews: impl IntoIterator<Item = Review>,
) -> HashMap<UserId, (ReviewState, DateTime<Utc>)> {
    let reviews_by_members = reviews
        .into_iter()
        // Filter to reviews by members and owners
        .filter(|it| {
            it.author_association
                .clone()
                .is_some_and(|it| it == AuthorAssociation::Member || it == AuthorAssociation::Owner)
        })
        // Filter and map to just the user, review state, and timestamp of the review
        .filter_map(|it| {
            it.user
                .zip(it.state.filter(|&it| {
                    it == ReviewState::Approved || it == ReviewState::ChangesRequested
                }))
                .zip(it.submitted_at)
                .map(|((u, s), d)| (u, s, d))
        });

    let mut latest_reviews_by_author = HashMap::new();
    for (user, state, datetime) in reviews_by_members {
        let entry = latest_reviews_by_author
            .entry(user.id)
            .or_insert((state, datetime));
        if entry.1 < datetime {
            *entry = (state, datetime);
        }
    }

    latest_reviews_by_author
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum MoffLabels {
    ChangesRequested,
    Approved,
    AwaitingReview,
}
impl MoffLabels {
    const APPROVED_LABEL: &str = "S: Approved";
    const AWAITING_REVIEW_LABEL: &str = "S: Needs Review";
    const CHANGES_REQUESTED_LABEL: &str = "S: Awaiting Changes";

    const fn to_label_string(self) -> &'static str {
        match self {
            Self::ChangesRequested => Self::CHANGES_REQUESTED_LABEL,
            Self::Approved => Self::APPROVED_LABEL,
            Self::AwaitingReview => Self::AWAITING_REVIEW_LABEL,
        }
    }
}
impl TryFrom<&str> for MoffLabels {
    type Error = ();

    fn try_from(value: &str) -> std::result::Result<Self, Self::Error> {
        match value {
            Self::CHANGES_REQUESTED_LABEL => Ok(Self::ChangesRequested),
            Self::APPROVED_LABEL => Ok(Self::Approved),
            Self::AWAITING_REVIEW_LABEL => Ok(Self::AwaitingReview),
            _ => Err(()),
        }
    }
}

#[cfg(test)]
mod tests {}
