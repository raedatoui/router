use std::collections::HashMap;
use std::ops::Add;
use std::ops::AddAssign;
use std::time::Duration;

use serde::Serialize;
use uuid::Uuid;

use super::duration_histogram::DurationHistogram;
use crate::spaceport::ReferencedFieldsForType;
use crate::spaceport::StatsContext;

#[derive(Default, Debug, Serialize)]
pub(crate) struct SingleStatsReport {
    pub(crate) request_id: Uuid,
    pub(crate) stats: HashMap<String, SingleStats>,
    pub(crate) operation_count: u64,
}

#[derive(Default, Debug, Serialize)]
pub(crate) struct SingleStats {
    pub(crate) stats_with_context: SingleContextualizedStats,
    pub(crate) referenced_fields_by_type: HashMap<String, ReferencedFieldsForType>,
}

#[derive(Default, Debug, Serialize)]
pub(crate) struct Stats {
    pub(crate) stats_with_context: ContextualizedStats,
    pub(crate) referenced_fields_by_type: HashMap<String, ReferencedFieldsForType>,
}

impl Add<SingleStats> for SingleStats {
    type Output = Stats;

    fn add(self, rhs: SingleStats) -> Self::Output {
        Stats {
            stats_with_context: self.stats_with_context + rhs.stats_with_context,
            // No merging required here because references fields by type will always be the same for each stats report key.
            referenced_fields_by_type: rhs.referenced_fields_by_type,
        }
    }
}

#[derive(Default, Debug, Serialize)]
pub(crate) struct SingleContextualizedStats {
    pub(crate) context: StatsContext,
    pub(crate) query_latency_stats: SingleQueryLatencyStats,
    pub(crate) per_type_stat: HashMap<String, SingleTypeStat>,
}

impl Add<SingleContextualizedStats> for SingleContextualizedStats {
    type Output = ContextualizedStats;

    fn add(self, stats: SingleContextualizedStats) -> Self::Output {
        let mut res = ContextualizedStats::default();
        res += self;
        res += stats;

        res
    }
}

// TODO Make some of these fields bool
#[derive(Default, Debug, Serialize)]
pub(crate) struct SingleQueryLatencyStats {
    pub(crate) latency: Duration,
    pub(crate) cache_hit: bool,
    pub(crate) persisted_query_hit: Option<bool>,
    pub(crate) cache_latency: Option<Duration>,
    pub(crate) root_error_stats: SinglePathErrorStats,
    pub(crate) has_errors: bool,
    pub(crate) public_cache_ttl_latency: Option<Duration>,
    pub(crate) private_cache_ttl_latency: Option<Duration>,
    pub(crate) registered_operation: bool,
    pub(crate) forbidden_operation: bool,
    pub(crate) without_field_instrumentation: bool,
}

impl Add<SingleQueryLatencyStats> for SingleQueryLatencyStats {
    type Output = QueryLatencyStats;
    fn add(self, stats: SingleQueryLatencyStats) -> Self::Output {
        let mut res = QueryLatencyStats::default();
        res += self;
        res += stats;

        res
    }
}

#[derive(Default, Debug, Serialize)]
pub(crate) struct SinglePathErrorStats {
    pub(crate) children: HashMap<String, SinglePathErrorStats>,
    pub(crate) errors_count: u64,
    pub(crate) requests_with_errors_count: u64,
}

#[derive(Default, Debug, Serialize)]
pub(crate) struct SingleTypeStat {
    pub(crate) per_field_stat: HashMap<String, SingleFieldStat>,
}

#[derive(Default, Debug, Serialize)]
pub(crate) struct SingleFieldStat {
    pub(crate) return_type: String,
    pub(crate) errors_count: u64,
    pub(crate) estimated_execution_count: f64,
    pub(crate) requests_with_errors_count: u64,
    pub(crate) latency: Duration,
}

#[derive(Default, Debug, Serialize)]
pub(crate) struct ContextualizedStats {
    context: StatsContext,
    query_latency_stats: QueryLatencyStats,
    per_type_stat: HashMap<String, TypeStat>,
}

impl AddAssign<SingleContextualizedStats> for ContextualizedStats {
    fn add_assign(&mut self, stats: SingleContextualizedStats) {
        self.context = stats.context;
        self.query_latency_stats += stats.query_latency_stats;
        for (k, v) in stats.per_type_stat {
            *self.per_type_stat.entry(k).or_default() += v;
        }
    }
}

#[derive(Default, Debug, Serialize)]
pub(crate) struct QueryLatencyStats {
    request_latencies: DurationHistogram,
    persisted_query_hits: u64,
    persisted_query_misses: u64,
    cache_hits: DurationHistogram,
    root_error_stats: PathErrorStats,
    requests_with_errors_count: u64,
    public_cache_ttl_count: DurationHistogram,
    private_cache_ttl_count: DurationHistogram,
    registered_operation_count: u64,
    forbidden_operation_count: u64,
    requests_without_field_instrumentation: u64,
}

impl AddAssign<SingleQueryLatencyStats> for QueryLatencyStats {
    fn add_assign(&mut self, stats: SingleQueryLatencyStats) {
        self.request_latencies
            .increment_duration(Some(stats.latency), 1);
        match stats.persisted_query_hit {
            Some(true) => self.persisted_query_hits += 1,
            Some(false) => self.persisted_query_misses += 1,
            None => {}
        }
        self.cache_hits.increment_duration(stats.cache_latency, 1);
        self.root_error_stats += stats.root_error_stats;
        self.requests_with_errors_count += stats.has_errors as u64;
        self.public_cache_ttl_count
            .increment_duration(stats.public_cache_ttl_latency, 1);
        self.private_cache_ttl_count
            .increment_duration(stats.private_cache_ttl_latency, 1);
        self.registered_operation_count += stats.registered_operation as u64;
        self.forbidden_operation_count += stats.forbidden_operation as u64;
        self.requests_without_field_instrumentation += stats.without_field_instrumentation as u64;
    }
}

#[derive(Default, Debug, Serialize)]
pub(crate) struct PathErrorStats {
    children: HashMap<String, PathErrorStats>,
    errors_count: u64,
    requests_with_errors_count: u64,
}

impl AddAssign<SinglePathErrorStats> for PathErrorStats {
    fn add_assign(&mut self, stats: SinglePathErrorStats) {
        for (k, v) in stats.children.into_iter() {
            *self.children.entry(k).or_default() += v;
        }
        self.errors_count += stats.errors_count;
        self.requests_with_errors_count += stats.requests_with_errors_count;
    }
}

#[derive(Default, Debug, Serialize)]
pub(crate) struct TypeStat {
    per_field_stat: HashMap<String, FieldStat>,
}

impl AddAssign<SingleTypeStat> for TypeStat {
    fn add_assign(&mut self, stat: SingleTypeStat) {
        for (k, v) in stat.per_field_stat.into_iter() {
            *self.per_field_stat.entry(k).or_default() += v;
        }
    }
}

#[derive(Default, Debug, Serialize)]
pub(crate) struct FieldStat {
    return_type: String,
    errors_count: u64,
    estimated_execution_count: f64,
    requests_with_errors_count: u64,
    latency: DurationHistogram,
}

impl AddAssign<SingleFieldStat> for FieldStat {
    fn add_assign(&mut self, stat: SingleFieldStat) {
        self.latency.increment_duration(Some(stat.latency), 1);
        self.requests_with_errors_count += stat.requests_with_errors_count;
        self.estimated_execution_count += stat.estimated_execution_count;
        self.errors_count += stat.errors_count;
        self.return_type = stat.return_type;
    }
}

impl From<ContextualizedStats> for crate::spaceport::ContextualizedStats {
    fn from(stats: ContextualizedStats) -> Self {
        Self {
            per_type_stat: stats
                .per_type_stat
                .into_iter()
                .map(|(k, v)| (k, v.into()))
                .collect(),
            query_latency_stats: Some(stats.query_latency_stats.into()),
            context: Some(stats.context),
        }
    }
}

impl From<QueryLatencyStats> for crate::spaceport::QueryLatencyStats {
    fn from(stats: QueryLatencyStats) -> Self {
        Self {
            latency_count: stats.request_latencies.buckets,
            request_count: stats.request_latencies.entries,
            cache_hits: stats.cache_hits.entries,
            cache_latency_count: stats.cache_hits.buckets,
            persisted_query_hits: stats.persisted_query_hits,
            persisted_query_misses: stats.persisted_query_misses,
            root_error_stats: Some(stats.root_error_stats.into()),
            requests_with_errors_count: stats.requests_with_errors_count,
            public_cache_ttl_count: stats.public_cache_ttl_count.buckets,
            private_cache_ttl_count: stats.private_cache_ttl_count.buckets,
            registered_operation_count: stats.registered_operation_count,
            forbidden_operation_count: stats.forbidden_operation_count,
            requests_without_field_instrumentation: stats.requests_without_field_instrumentation,
        }
    }
}

impl From<PathErrorStats> for crate::spaceport::PathErrorStats {
    fn from(stats: PathErrorStats) -> Self {
        Self {
            children: stats
                .children
                .into_iter()
                .map(|(k, v)| (k, v.into()))
                .collect(),
            errors_count: stats.errors_count,
            requests_with_errors_count: stats.requests_with_errors_count,
        }
    }
}

impl From<TypeStat> for crate::spaceport::TypeStat {
    fn from(stat: TypeStat) -> Self {
        Self {
            per_field_stat: stat
                .per_field_stat
                .into_iter()
                .map(|(k, v)| (k, v.into()))
                .collect(),
        }
    }
}

impl From<FieldStat> for crate::spaceport::FieldStat {
    fn from(stat: FieldStat) -> Self {
        Self {
            return_type: stat.return_type,
            errors_count: stat.errors_count,
            observed_execution_count: stat.latency.entries,
            estimated_execution_count: stat.estimated_execution_count as u64,
            requests_with_errors_count: stat.requests_with_errors_count,
            latency_count: stat.latency.buckets,
        }
    }
}

#[cfg(test)]
mod test {
    use std::collections::HashMap;
    use std::time::Duration;

    use super::*;
    use crate::plugins::telemetry::apollo::Report;
    use crate::spaceport::ReferencedFieldsForType;

    #[test]
    fn test_aggregation() {
        let metric_1 = create_test_metric("client_1", "version_1", "report_key_1");
        let metric_2 = create_test_metric("client_1", "version_1", "report_key_1");
        let aggregated_metrics = Report::new(vec![metric_1, metric_2]);

        insta::with_settings!({sort_maps => true}, {
            insta::assert_json_snapshot!(aggregated_metrics);
        });
    }

    #[test]
    fn test_aggregation_grouping() {
        let metric_1 = create_test_metric("client_1", "version_1", "report_key_1");
        let metric_2 = create_test_metric("client_1", "version_1", "report_key_1");
        let metric_3 = create_test_metric("client_2", "version_1", "report_key_1");
        let metric_4 = create_test_metric("client_1", "version_2", "report_key_1");
        let metric_5 = create_test_metric("client_1", "version_1", "report_key_2");
        let aggregated_metrics =
            Report::new(vec![metric_1, metric_2, metric_3, metric_4, metric_5]);
        assert_eq!(aggregated_metrics.traces_per_query.len(), 2);
        assert_eq!(
            aggregated_metrics.traces_per_query["report_key_1"]
                .stats_with_context
                .len(),
            3
        );
        assert_eq!(
            aggregated_metrics.traces_per_query["report_key_2"]
                .stats_with_context
                .len(),
            1
        );
    }

    fn create_test_metric(
        client_name: &str,
        client_version: &str,
        stats_report_key: &str,
    ) -> SingleStatsReport {
        // This makes me sad. Really this should have just been a case of generate a couple of metrics using
        // a prop testing library and then assert that things got merged OK. But in practise everything was too hard to use

        let mut count = Count::default();

        SingleStatsReport {
            request_id: Uuid::default(),
            operation_count: count.inc_u64(),
            stats: HashMap::from([(
                stats_report_key.to_string(),
                SingleStats {
                    stats_with_context: SingleContextualizedStats {
                        context: StatsContext {
                            client_name: client_name.to_string(),
                            client_version: client_version.to_string(),
                        },
                        query_latency_stats: SingleQueryLatencyStats {
                            latency: Duration::from_secs(1),
                            cache_hit: true,
                            persisted_query_hit: Some(true),
                            cache_latency: Some(Duration::from_secs(1)),
                            root_error_stats: SinglePathErrorStats {
                                children: HashMap::from([(
                                    "path1".to_string(),
                                    SinglePathErrorStats {
                                        children: HashMap::from([(
                                            "path2".to_string(),
                                            SinglePathErrorStats {
                                                children: Default::default(),
                                                errors_count: count.inc_u64(),
                                                requests_with_errors_count: count.inc_u64(),
                                            },
                                        )]),
                                        errors_count: count.inc_u64(),
                                        requests_with_errors_count: count.inc_u64(),
                                    },
                                )]),
                                errors_count: count.inc_u64(),
                                requests_with_errors_count: count.inc_u64(),
                            },
                            has_errors: true,
                            public_cache_ttl_latency: Some(Duration::from_secs(1)),
                            private_cache_ttl_latency: Some(Duration::from_secs(1)),
                            registered_operation: true,
                            forbidden_operation: true,
                            without_field_instrumentation: true,
                        },
                        per_type_stat: HashMap::from([
                            (
                                "type1".into(),
                                SingleTypeStat {
                                    per_field_stat: HashMap::from([
                                        ("field1".into(), field_stat(&mut count)),
                                        ("field2".into(), field_stat(&mut count)),
                                    ]),
                                },
                            ),
                            (
                                "type2".into(),
                                SingleTypeStat {
                                    per_field_stat: HashMap::from([
                                        ("field1".into(), field_stat(&mut count)),
                                        ("field2".into(), field_stat(&mut count)),
                                    ]),
                                },
                            ),
                        ]),
                    },
                    referenced_fields_by_type: HashMap::from([(
                        "type1".into(),
                        ReferencedFieldsForType {
                            field_names: vec!["field1".into(), "field2".into()],
                            is_interface: false,
                        },
                    )]),
                },
            )]),
        }
    }

    fn field_stat(count: &mut Count) -> SingleFieldStat {
        SingleFieldStat {
            return_type: "String".into(),
            errors_count: count.inc_u64(),
            estimated_execution_count: count.inc_f64(),
            requests_with_errors_count: count.inc_u64(),
            latency: Duration::from_secs(1),
        }
    }

    #[derive(Default)]
    struct Count {
        count: u64,
    }
    impl Count {
        fn inc_u64(&mut self) -> u64 {
            self.count += 1;
            self.count
        }
        fn inc_f64(&mut self) -> f64 {
            self.count += 1;
            self.count as f64
        }
    }
}
