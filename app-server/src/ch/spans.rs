use anyhow::Result;
use chrono::{DateTime, Utc};
use clickhouse::Row;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{ch::utils::round_small_values_to_zero, db, traces::SpanUsage};

use super::{
    modifiers::GroupByInterval,
    utils::{chrono_to_nanoseconds, TimeBounds},
    Aggregation,
};

#[derive(Row, Serialize, Deserialize)]
pub struct CHSpan {
    #[serde(with = "clickhouse::serde::uuid")]
    pub span_id: Uuid,
    pub name: String,
    pub span_type: u8,
    /// Start time in nanoseconds
    pub start_time: i64,
    /// End time in nanoseconds
    pub end_time: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    pub input_cost: f64,
    pub output_cost: f64,
    pub total_cost: f64,
    pub model: String,
    pub session_id: String,
    #[serde(with = "clickhouse::serde::uuid")]
    pub project_id: Uuid,
    #[serde(with = "clickhouse::serde::uuid")]
    pub trace_id: Uuid,
    pub provider: String,
    pub user_id: String,
}

impl CHSpan {
    pub fn from_db_span(span: &db::trace::Span, usage: SpanUsage, project_id: Uuid) -> Self {
        let span_attributes = span.get_attributes();

        CHSpan {
            span_id: span.span_id,
            name: span.name.clone(),
            span_type: span.span_type.clone().into(),
            start_time: chrono_to_nanoseconds(span.start_time),
            end_time: chrono_to_nanoseconds(span.end_time),
            prompt_tokens: usage.prompt_tokens,
            completion_tokens: usage.completion_tokens,
            total_tokens: usage.total_tokens,
            input_cost: usage.input_cost,
            output_cost: usage.output_cost,
            total_cost: usage.total_cost,
            model: usage.model.unwrap_or(String::from("<null>")),
            session_id: span_attributes
                .session_id()
                .unwrap_or(String::from("<null>")),
            project_id: project_id,
            trace_id: span.trace_id,
            provider: usage.provider_name.unwrap_or(String::from("<null>")),
            user_id: span_attributes.user_id().unwrap_or(String::from("<null>")),
        }
    }
}

pub async fn insert_span(clickhouse: clickhouse::Client, span: &CHSpan) -> Result<()> {
    let ch_insert = clickhouse.insert("spans");
    match ch_insert {
        Ok(mut ch_insert) => {
            ch_insert.write(span).await?;
            let ch_insert_end_res = ch_insert.end().await;
            match ch_insert_end_res {
                Ok(_) => Ok(()),
                Err(e) => {
                    return Err(anyhow::anyhow!("Clickhouse span insertion failed: {:?}", e));
                }
            }
        }
        Err(e) => {
            return Err(anyhow::anyhow!(
                "Failed to insert span into Clickhouse: {:?}",
                e
            ));
        }
    }
}

#[derive(Deserialize, Row, Serialize, Debug)]
pub struct IntMetricTimeValue {
    pub time: u32,
    pub value: i64,
}

#[derive(Deserialize, Row, Serialize)]
pub struct FloatMetricTimeValue {
    pub time: u32,
    pub value: f64,
}

pub async fn get_time_bounds(
    clickhouse: clickhouse::Client,
    project_id: Uuid,
) -> Result<TimeBounds> {
    let query_string = format!(
        "SELECT
            MIN(start_time) AS min_time,
            MAX(start_time) AS max_time
        FROM
            spans
        WHERE project_id = '{}'",
        project_id
    );

    let mut cursor = clickhouse.query(&query_string).fetch::<TimeBounds>()?;

    let time_bounds = cursor.next().await?.unwrap();
    Ok(time_bounds)
}

pub async fn get_total_trace_count_metrics_relative(
    clickhouse: clickhouse::Client,
    group_by_interval: GroupByInterval,
    project_id: Uuid,
    past_hours: i64,
) -> Result<Vec<IntMetricTimeValue>> {
    let ch_round_time = group_by_interval.to_ch_truncate_time();
    let ch_interval = group_by_interval.to_interval();
    let ch_step = group_by_interval.to_ch_step();

    let query_string = format!(
        "
    WITH traces AS (
        SELECT
            trace_id,
            project_id,
            {}(MIN(start_time)) as time
        FROM spans
        GROUP BY project_id, trace_id
    )
    SELECT
        time,
        COUNT(DISTINCT(trace_id)) as value
    FROM traces
    WHERE
        project_id = '{}'
        AND time >= now() - INTERVAL {} HOUR
    GROUP BY
        time
    ORDER BY
        time
    WITH FILL
    FROM {}(NOW() - INTERVAL {} HOUR + INTERVAL {})
    TO {}(NOW() + INTERVAL {})
    STEP {}",
        ch_round_time,
        project_id,
        past_hours,
        ch_round_time,
        past_hours,
        ch_interval,
        ch_round_time,
        ch_interval,
        ch_step
    );

    let mut cursor = clickhouse
        .query(&query_string)
        .fetch::<IntMetricTimeValue>()?;

    let mut res = Vec::new();
    while let Some(row) = cursor.next().await? {
        res.push(row);
    }

    Ok(res)
}

pub async fn get_total_trace_count_metrics_absolute(
    clickhouse: clickhouse::Client,
    group_by_interval: GroupByInterval,
    project_id: Uuid,
    start_time: DateTime<Utc>,
    end_time: DateTime<Utc>,
) -> Result<Vec<IntMetricTimeValue>> {
    let ch_round_time = group_by_interval.to_ch_truncate_time();
    let ch_step = group_by_interval.to_ch_step();
    let ch_start_time = start_time.timestamp();
    let ch_end_time = end_time.timestamp();

    let query_string = format!(
        "
    WITH traces AS (
    SELECT
        trace_id,
        project_id,
        {}(MIN(start_time)) as time,
        SUM(total_tokens) as value
    FROM spans
    GROUP BY project_id, trace_id
    )
    SELECT
        time,
        COUNT(DISTINCT(trace_id)) as value
    FROM traces
    WHERE
        project_id = '{}'
        AND time >= fromUnixTimestamp({})
        AND time <= fromUnixTimestamp({})
    GROUP BY
        time
    ORDER BY
        time
    WITH FILL
    FROM {}(fromUnixTimestamp({}))
    TO {}(fromUnixTimestamp({}))
    STEP {}",
        ch_round_time,
        project_id,
        ch_start_time,
        ch_end_time,
        ch_round_time,
        ch_start_time,
        ch_round_time,
        ch_end_time,
        ch_step
    );

    let mut cursor = clickhouse
        .query(&query_string)
        .fetch::<IntMetricTimeValue>()?;

    let mut res = Vec::new();
    while let Some(row) = cursor.next().await? {
        res.push(row);
    }

    Ok(res)
}

pub async fn get_trace_latency_seconds_metrics_relative(
    clickhouse: clickhouse::Client,
    group_by_interval: GroupByInterval,
    project_id: Uuid,
    past_hours: i64,
    aggregation: Aggregation,
) -> Result<Vec<FloatMetricTimeValue>> {
    let ch_round_time = group_by_interval.to_ch_truncate_time();
    let ch_interval = group_by_interval.to_interval();
    let ch_step = group_by_interval.to_ch_step();
    let ch_aggregation = aggregation.to_ch_agg_function();

    let query_string = format!(
        "
    WITH traces AS (
    SELECT
        trace_id,
        project_id,
        {}(MIN(start_time)) as time,
        toUnixTimestamp64Nano(MAX(end_time)) - toUnixTimestamp64Nano(MIN(start_time)) as value
    FROM spans
    GROUP BY project_id, trace_id
    )
    SELECT
        time,
        {}(value) as value
    FROM traces
    WHERE
        project_id = '{}'
        AND time >= now() - INTERVAL {} HOUR
    GROUP BY
        time
    ORDER BY
        time
    WITH FILL
    FROM {}(NOW() - INTERVAL {} HOUR + INTERVAL {})
    TO {}(NOW() + INTERVAL {})
    STEP {}",
        ch_round_time,
        ch_aggregation,
        project_id,
        past_hours,
        ch_round_time,
        past_hours,
        ch_interval,
        ch_round_time,
        ch_interval,
        ch_step
    );

    let mut cursor = clickhouse
        .query(&query_string)
        .fetch::<FloatMetricTimeValue>()?;

    let mut res = Vec::new();
    while let Some(row) = cursor.next().await? {
        res.push(row);
    }

    // TODO: Move this logic to Clickhouse query
    let res = res
        .into_iter()
        .map(|value| FloatMetricTimeValue {
            time: value.time,
            value: {
                let value_sec = value.value as f64 / 1_000_000_000.0;
                round_small_values_to_zero(value_sec)
            },
        })
        .collect();

    Ok(res)
}

pub async fn get_trace_latency_seconds_metrics_absolute(
    clickhouse: clickhouse::Client,
    group_by_interval: GroupByInterval,
    project_id: Uuid,
    start_time: DateTime<Utc>,
    end_time: DateTime<Utc>,
    aggregation: Aggregation,
) -> Result<Vec<FloatMetricTimeValue>> {
    let ch_round_time = group_by_interval.to_ch_truncate_time();
    let ch_step = group_by_interval.to_ch_step();
    let ch_start_time = start_time.timestamp();
    let ch_end_time = end_time.timestamp();
    let ch_aggregation = aggregation.to_ch_agg_function();

    let query_string = format!(
        "
        WITH traces AS (
        SELECT
            trace_id,
            project_id,
            {}(MIN(start_time)) as time,
            toUnixTimestamp64Nano(MAX(end_time)) - toUnixTimestamp64Nano(MIN(start_time)) as value
        FROM spans
        GROUP BY project_id, trace_id
        )
        SELECT
            time,
            {}(value) as value
        FROM traces
        WHERE
            project_id = '{}'
            AND time >= fromUnixTimestamp({})
            AND time <= fromUnixTimestamp({})
        GROUP BY
            time
        ORDER BY
            time
        WITH FILL
        FROM {}(fromUnixTimestamp({}))
        TO {}(fromUnixTimestamp({}))
        STEP {}",
        ch_round_time,
        ch_aggregation,
        project_id,
        ch_start_time,
        ch_end_time,
        ch_round_time,
        ch_start_time,
        ch_round_time,
        ch_end_time,
        ch_step
    );

    let mut cursor = clickhouse
        .query(&query_string)
        .fetch::<FloatMetricTimeValue>()?;

    let mut res = Vec::new();
    while let Some(row) = cursor.next().await? {
        res.push(row);
    }

    // TODO: Move this logic to Clickhouse query
    let res = res
        .into_iter()
        .map(|value| FloatMetricTimeValue {
            time: value.time,
            value: {
                let value_sec = value.value as f64 / 1_000_000_000.0;
                round_small_values_to_zero(value_sec)
            },
        })
        .collect();

    Ok(res)
}

pub async fn get_total_token_count_metrics_relative(
    clickhouse: clickhouse::Client,
    group_by_interval: GroupByInterval,
    project_id: Uuid,
    past_hours: i64,
    aggregation: Aggregation,
) -> Result<Vec<IntMetricTimeValue>> {
    let ch_round_time = group_by_interval.to_ch_truncate_time();
    let ch_interval = group_by_interval.to_interval();
    let ch_step = group_by_interval.to_ch_step();
    let ch_aggregation = aggregation.to_ch_agg_function();

    let query_string = format!(
        "
    WITH traces AS (
    SELECT
        trace_id,
        project_id,
        {}(MIN(start_time)) as time,
        SUM(total_tokens) as value
    FROM spans
    GROUP BY project_id, trace_id
    )
    SELECT
        time,
        {}(value) as value
    FROM traces
    WHERE
        project_id = '{}'
        AND time >= now() - INTERVAL {} HOUR
    GROUP BY
        time
    ORDER BY
        time
    WITH FILL
    FROM {}(NOW() - INTERVAL {} HOUR + INTERVAL {})
    TO {}(NOW() + INTERVAL {})
    STEP {}",
        ch_round_time,
        ch_aggregation,
        project_id,
        past_hours,
        ch_round_time,
        past_hours,
        ch_interval,
        ch_round_time,
        ch_interval,
        ch_step
    );

    let mut cursor = clickhouse
        .query(&query_string)
        .fetch::<IntMetricTimeValue>()?;

    let mut res = Vec::new();
    while let Some(row) = cursor.next().await? {
        res.push(row);
    }

    Ok(res)
}

pub async fn get_total_token_count_metrics_absolute(
    clickhouse: clickhouse::Client,
    group_by_interval: GroupByInterval,
    project_id: Uuid,
    start_time: DateTime<Utc>,
    end_time: DateTime<Utc>,
    aggregation: Aggregation,
) -> Result<Vec<IntMetricTimeValue>> {
    let ch_round_time = group_by_interval.to_ch_truncate_time();
    let ch_step = group_by_interval.to_ch_step();
    let ch_start_time = start_time.timestamp();
    let ch_end_time = end_time.timestamp();
    let ch_aggregation = aggregation.to_ch_agg_function();

    let query_string = format!(
        "
    WITH traces AS (
    SELECT
        trace_id,
        project_id,
        {}(MIN(start_time)) as time,
        SUM(total_tokens) as value
    FROM spans
    GROUP BY project_id, trace_id
    )
    SELECT
        time,
        {}(value) as value
    FROM traces
    WHERE
        project_id = '{}'
        AND time >= fromUnixTimestamp({})
        AND time <= fromUnixTimestamp({})
    GROUP BY
        time
    ORDER BY
        time
    WITH FILL
    FROM {}(fromUnixTimestamp({}))
    TO {}(fromUnixTimestamp({}))
    STEP {}",
        ch_round_time,
        ch_aggregation,
        project_id,
        ch_start_time,
        ch_end_time,
        ch_round_time,
        ch_start_time,
        ch_round_time,
        ch_end_time,
        ch_step
    );

    let mut cursor = clickhouse
        .query(&query_string)
        .fetch::<IntMetricTimeValue>()?;

    let mut res = Vec::new();
    while let Some(row) = cursor.next().await? {
        res.push(row);
    }

    Ok(res)
}

pub async fn get_cost_usd_metrics_relative(
    clickhouse: clickhouse::Client,
    group_by_interval: GroupByInterval,
    project_id: Uuid,
    past_hours: i64,
    aggregation: Aggregation,
) -> Result<Vec<FloatMetricTimeValue>> {
    let ch_round_time = group_by_interval.to_ch_truncate_time();
    let ch_interval = group_by_interval.to_interval();
    let ch_step = group_by_interval.to_ch_step();
    let ch_aggregation = aggregation.to_ch_agg_function();

    let query_string = format!(
        "
    WITH traces AS (
    SELECT
        trace_id,
        project_id,
        {}(MIN(start_time)) as time,
        SUM(total_cost) as value
    FROM spans
    GROUP BY project_id, trace_id
    )
    SELECT
        time,
        {}(value) as value
    FROM traces
    WHERE
        project_id = '{}'
        AND time >= now() - INTERVAL {} HOUR
    GROUP BY
        time
    ORDER BY
        time
    WITH FILL
    FROM {}(NOW() - INTERVAL {} HOUR + INTERVAL {})
    TO {}(NOW() + INTERVAL {})
    STEP {}",
        ch_round_time,
        ch_aggregation,
        project_id,
        past_hours,
        ch_round_time,
        past_hours,
        ch_interval,
        ch_round_time,
        ch_interval,
        ch_step
    );

    let mut cursor = clickhouse
        .query(&query_string)
        .fetch::<FloatMetricTimeValue>()?;

    let mut res = Vec::new();
    while let Some(row) = cursor.next().await? {
        res.push(row);
    }

    Ok(res)
}

pub async fn get_cost_usd_metrics_absolute(
    clickhouse: clickhouse::Client,
    group_by_interval: GroupByInterval,
    project_id: Uuid,
    start_time: DateTime<Utc>,
    end_time: DateTime<Utc>,
    aggregation: Aggregation,
) -> Result<Vec<FloatMetricTimeValue>> {
    let ch_round_time = group_by_interval.to_ch_truncate_time();
    let ch_step = group_by_interval.to_ch_step();
    let ch_start_time = start_time.timestamp();
    let ch_end_time = end_time.timestamp();
    let ch_aggregation = aggregation.to_ch_agg_function();

    let query_string = format!(
        "
        WITH traces AS (
        SELECT
            trace_id,
            project_id,
            {}(MIN(start_time)) as time,
            SUM(total_cost) as value
        FROM spans
        GROUP BY project_id, trace_id
        )
        SELECT
            time,
            {}(value) as value
        FROM traces
        WHERE
            project_id = '{}'
            AND time >= fromUnixTimestamp({})
            AND time <= fromUnixTimestamp({})
        GROUP BY
            time
        ORDER BY
            time
        WITH FILL
        FROM {}(fromUnixTimestamp({}))
        TO {}(fromUnixTimestamp({}))
        STEP {}",
        ch_round_time,
        ch_aggregation,
        project_id,
        ch_start_time,
        ch_end_time,
        ch_round_time,
        ch_start_time,
        ch_round_time,
        ch_end_time,
        ch_step
    );

    let mut cursor = clickhouse
        .query(&query_string)
        .fetch::<FloatMetricTimeValue>()?;

    let mut res = Vec::new();
    while let Some(row) = cursor.next().await? {
        res.push(row);
    }

    Ok(res)
}
