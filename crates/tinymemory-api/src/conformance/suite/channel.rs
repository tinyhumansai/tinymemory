//! Channel filtering across every read and forget operation.

use crate::{ExploreRequest, Facet, ForgetTarget, MetaFilter, RecallRequest};

use super::{Ctx, ensure};
use crate::conformance::error::Result;

/// A channel-scoped read only sees items from that channel, and forgetting it
/// leaves items from other channels alone.
pub(super) async fn channel(ctx: &Ctx<'_>) -> Result<()> {
    const CHECK: &str = "channel";
    let channel = format!("{}-channel", ctx.run.marker);
    let other_channel = format!("{}-other-channel", ctx.run.marker);
    let tag = format!("{}-channel-check", ctx.run.marker);
    let mut channel_meta = ctx.run.meta();
    channel_meta.channel = Some(channel.clone());
    channel_meta.tags.push(tag.clone());
    let wanted = ctx
        .call(
            CHECK,
            ctx.engine
                .store(ctx.run.document("channel-scoped note", channel_meta)),
        )
        .await?
        .id;
    let mut other_meta = ctx.run.meta();
    other_meta.channel = Some(other_channel);
    other_meta.tags.push(tag.clone());
    let other = ctx
        .call(
            CHECK,
            ctx.engine
                .store(ctx.run.document("other-channel note", other_meta)),
        )
        .await?
        .id;
    let filter = MetaFilter {
        channel: Some(channel),
        ..ctx.run.filter()
    };

    let listed = ctx.list_all(CHECK, &filter).await?;
    ensure(CHECK, listed.len() == 1 && listed[0].id == wanted, || {
        format!(
            "channel list returned {:?}, expected only {wanted}",
            listed.iter().map(|hit| &hit.id).collect::<Vec<_>>()
        )
    })?;

    let mut recall = RecallRequest::new(ctx.run.marker.clone(), 10);
    recall.filter = filter.clone();
    let answer = ctx.call(CHECK, ctx.engine.recall(recall)).await?;
    ensure(
        CHECK,
        answer.citations.len() == 1 && answer.citations[0].id == wanted,
        || {
            format!(
                "channel recall cited {:?}, expected only {wanted}",
                answer
                    .citations
                    .iter()
                    .map(|citation| &citation.id)
                    .collect::<Vec<_>>()
            )
        },
    )?;

    let mut explore = ExploreRequest::new(Facet::Kind, 10);
    explore.filter = filter.clone();
    let page = ctx.call(CHECK, ctx.engine.explore(explore)).await?;
    ensure(
        CHECK,
        page.total == 1 && page.buckets.iter().map(|bucket| bucket.count).sum::<u64>() == 1,
        || format!("channel explore returned {page:?}"),
    )?;

    let report = ctx
        .call(
            CHECK,
            ctx.engine.forget(ForgetTarget::Filter(filter.clone())),
        )
        .await?;
    ensure(CHECK, report.forgotten == 1, || {
        format!(
            "channel forget removed {} items, expected one",
            report.forgotten
        )
    })?;
    let remaining_filter = MetaFilter {
        tags_any: vec![tag],
        ..ctx.run.filter()
    };
    let remaining = ctx.list_all(CHECK, &remaining_filter).await?;
    ensure(
        CHECK,
        remaining.len() == 1 && remaining[0].id == other,
        || {
            format!(
                "channel forget left {:?}, expected only {other}",
                remaining.iter().map(|hit| &hit.id).collect::<Vec<_>>()
            )
        },
    )
}
