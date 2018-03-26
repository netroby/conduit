use std::default::Default;
use std::{fmt, u32};
use std::hash::Hash;
use std::sync::{Arc, Mutex};

use futures::future::{self, FutureResult};
use http;
use hyper;
use hyper::header::{ContentLength, ContentType};
use hyper::StatusCode;
use hyper::server::{
    Service as HyperService,
    Request as HyperRequest,
    Response as HyperResponse
};
use indexmap::{IndexMap};

use ctx;
use telemetry::event::Event;
use super::latency::{BUCKET_BOUNDS, Histogram};

#[derive(Debug, Clone)]
struct Metrics {
    request_total: Metric<Counter, Arc<RequestLabels>>,
    request_duration: Metric<Histogram, Arc<RequestLabels>>,

    response_total: Metric<Counter, Arc<ResponseLabels>>,
    response_duration: Metric<Histogram, Arc<ResponseLabels>>,
    response_latency: Metric<Histogram, Arc<ResponseLabels>>,
}

#[derive(Debug, Clone)]
struct Metric<M, L: Hash + Eq> {
    name: &'static str,
    help: &'static str,

    /// Labels from the `CONDUIT_PROMETHEUS_LABELS` environment variable.
    ///
    /// Since this should be applied to all metrics and never changes
    /// over the lifetime of the process, we can store it at the `Metric` level
    /// rather than in the `Labels` of each individual value. This should keep
    /// the ref count much smaller, and means we don't have to factor it into
    /// labels hashing.
    env_labels: Option<Arc<str>>,

    values: IndexMap<L, M>
}

#[derive(Copy, Debug, Default, Clone, Eq, PartialEq)]
struct Counter(u64);

/// Tracks Prometheus metrics
#[derive(Debug)]
pub struct Aggregate {
    metrics: Arc<Mutex<Metrics>>,
}


/// Serve Prometheues metrics.
#[derive(Debug, Clone)]
pub struct Serve {
    metrics: Arc<Mutex<Metrics>>,
}

/// Construct the Prometheus metrics.
///
/// Returns the `Aggregate` and `Serve` sides. The `Serve` side
/// is a Hyper service which can be used to create the server for the
/// scrape endpoint, while the `Aggregate` side can receive updates to the
/// metrics by calling `record_event`.
pub fn new(env_labels: Option<Arc<str>>) -> (Aggregate, Serve) {
    let metrics = Arc::new(Mutex::new(Metrics::new(env_labels)));
    (Aggregate::new(&metrics), Serve::new(&metrics))
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Hash)]
struct RequestLabels {

    outbound_labels: Option<OutboundLabels>,

    /// The value of the `:authority` (HTTP/2) or `Host` (HTTP/1.1) header of
    /// the request.
    authority: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Hash)]
struct ResponseLabels {

    request_labels: RequestLabels,

    /// The HTTP status code of the response.
    status_code: u16,

    /// The value of the grpc-status trailer. Only applicable to response
    /// metrics for gRPC responses.
    grpc_status_code: Option<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
// TODO: when #429 is done, this will no longer be dead code.
#[allow(dead_code)]
enum PodOwner {
    /// The deployment to which this request is being sent.
    Deployment(String),

    /// The job to which this request is being sent.
    Job(String),

    /// The replica set to which this request is being sent.
    ReplicaSet(String),

    /// The replication controller to which this request is being sent.
    ReplicationController(String),
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Hash)]
struct OutboundLabels {
    /// The owner of the destination pod.
    //  TODO: when #429 is done, this will no longer need to be an Option.
    dst: Option<PodOwner>,

    ///  The namespace to which this request is being sent (if
    /// applicable).
    namespace: Option<String>
}

// ===== impl Metrics =====

impl Metrics {

    pub fn new(env_labels: Option<Arc<str>>) -> Self {

        let request_total = Metric::<Counter, Arc<RequestLabels>>::new(
            "request_total",
            "A counter of the number of requests the proxy has received.",
            env_labels.clone(),
        );

        let request_duration = Metric::<Histogram, Arc<RequestLabels>>::new(
            "request_duration_ms",
            "A histogram of the duration of a request. This is measured from \
             when the request headers are received to when the request \
             stream has completed.",
            env_labels.clone(),
        );

        let response_total = Metric::<Counter, Arc<ResponseLabels>>::new(
            "response_total",
            "A counter of the number of responses the proxy has received.",
            env_labels.clone(),
        );

        let response_duration = Metric::<Histogram, Arc<ResponseLabels>>::new(
            "response_duration_ms",
            "A histogram of the duration of a response. This is measured from \
             when the response headers are received to when the response \
             stream has completed.",

            env_labels.clone(),
        );

        let response_latency = Metric::<Histogram, Arc<ResponseLabels>>::new(
            "response_latency_ms",
            "A histogram of the total latency of a response. This is measured \
            from when the request headers are received to when the response \
            stream has completed.",
            env_labels.clone(),
        );

        Metrics {
            request_total,
            request_duration,
            response_total,
            response_duration,
            response_latency,
        }
    }

    fn request_total(&mut self, labels: &Arc<RequestLabels>) -> &mut u64 {
        &mut self.request_total.values
            .entry(labels.clone())
            .or_insert_with(Default::default).0
    }

    fn request_duration(&mut self,
                        labels: &Arc<RequestLabels>)
                        -> &mut Histogram {
        self.request_duration.values
            .entry(labels.clone())
            .or_insert_with(Default::default)
    }

    fn response_duration(&mut self,
                         labels: &Arc<ResponseLabels>)
                         -> &mut Histogram {
        self.response_duration.values
            .entry(labels.clone())
            .or_insert_with(Default::default)
    }

    fn response_latency(&mut self,
                        labels: &Arc<ResponseLabels>)
                        -> &mut Histogram {
        self.response_latency.values
            .entry(labels.clone())
            .or_insert_with(Default::default)
    }

    fn response_total(&mut self, labels: &Arc<ResponseLabels>) -> &mut u64 {
        &mut self.response_total.values
            .entry(labels.clone())
            .or_insert_with(Default::default).0
    }
}


impl fmt::Display for Metrics {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}\n{}\n{}\n{}\n{}",
            self.request_total,
            self.request_duration,
            self.response_total,
            self.response_duration,
            self.response_latency,
        )
    }
}

impl fmt::Display for Counter {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0 as f64)
    }
}

// ===== impl Metric =====

impl<M, L: Hash + Eq> Metric<M, L> {

    pub fn new(name: &'static str,
               help: &'static str,
               env_labels: Option<Arc<str>>)
               -> Self {
        Metric {
            name,
            help,
            env_labels,
            values: IndexMap::new(),
        }
    }

}

impl<L> fmt::Display for Metric<Counter, L>
where
    L: fmt::Display,
    L: Hash + Eq,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f,
            "# HELP {name} {help}\n# TYPE {name} counter\n",
            name = self.name,
            help = self.help,
        )?;

        let comma = if self.env_labels.is_some() {
            ","
        } else {
            ""
        };

        let env_labels = self.env_labels.as_ref()
            .map_or("", AsRef::as_ref);

        for (labels, value) in &self.values {
            write!(f, "{name}{{{env_labels}{comma}{labels}}} {value}\n",
                name = self.name,
                env_labels = env_labels,
                comma = comma,
                labels = labels,
                value = value,
            )?;
        }

        Ok(())
    }
}

impl<L> fmt::Display for Metric<Histogram, L> where
    L: fmt::Display,
    L: Hash + Eq,
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f,
            "# HELP {name} {help}\n# TYPE {name} histogram\n",
            name = self.name,
            help = self.help,
        )?;

        // determine whether or not to place a comma after the labels from
        // the environment variable.
        let env_comma = if self.env_labels.is_some() {
            ","
        } else {
            ""
        };
        let env_labels = self.env_labels.as_ref()
            .map_or("", AsRef::as_ref);

        for (labels, histogram) in &self.values {

            // Look up the bucket numbers against the BUCKET_BOUNDS array
            // to turn them into upper bounds.
            let bounds_and_counts = histogram.into_iter()
                .enumerate()
                .map(|(num, count)| (BUCKET_BOUNDS[num], count));

            // Since Prometheus expects each bucket's value to be the sum of
            // the number of values in this bucket and all lower buckets,
            // track the total count here.
            let mut total_count = 0;
            for (le, count) in bounds_and_counts {
                // Add this bucket's count to the total count.
                total_count += count;
                write!(f,
                    "{name}_bucket{{{env_labels}{comma}{labels},le=\"{le}\"}} {count}\n",
                    name = self.name,
                    env_labels = env_labels,
                    comma = env_comma,
                    labels = labels,
                    le = le,
                    // Print the total count *as of this iteration*.
                    count = total_count,
                )?;
            }

            // Print the total count and histogram sum stats.
            write!(f,
                "{name}_count{{{env_labels}{comma}{labels}}} {count}\n\
                 {name}_sum{{{env_labels}{comma}{labels}}} {sum}\n",
                name = self.name,
                env_labels = env_labels,
                comma = env_comma,
                labels = labels,
                count = total_count,
                sum = histogram.sum_in_ms(),
            )?;
        }

        Ok(())
    }
}

// ===== impl Aggregate =====

impl Aggregate {

    fn new(metrics: &Arc<Mutex<Metrics>>) -> Self {
        Aggregate {
            metrics: metrics.clone(),
        }
    }

    #[inline]
    fn update<F: Fn(&mut Metrics)>(&mut self, f: F) {
        let mut lock = self.metrics.lock()
            .expect("metrics lock poisoned");
        f(&mut *lock);
    }

    /// Observe the given event.
    pub fn record_event(&mut self, event: &Event) {
        trace!("Metrics::record({:?})", event);
        match *event {

            Event::StreamRequestOpen(_) | Event::StreamResponseOpen(_, _) => {
                // Do nothing; we'll record metrics for the request or response
                //  when the stream *finishes*.
            },

            Event::StreamRequestFail(ref req, ref fail) => {
                let labels = Arc::new(RequestLabels::new(req));
                self.update(|metrics| {
                    *metrics.request_total(&labels) += 1;
                    *metrics.request_duration(&labels) +=
                        fail.since_request_open;
                })
            },

            Event::StreamRequestEnd(ref req, ref end) => {
                let labels = Arc::new(RequestLabels::new(req));
                self.update(|metrics| {
                    *metrics.request_total(&labels) += 1;
                    *metrics.request_duration(&labels) +=
                        end.since_request_open;
                })
            },

            Event::StreamResponseEnd(ref res, ref end) => {
                let labels = Arc::new(ResponseLabels::new(
                    res,
                    end.grpc_status,
                ));
                self.update(|metrics| {
                    *metrics.response_total(&labels) += 1;
                    *metrics.response_duration(&labels) +=  end.since_response_open;
                    *metrics.response_latency(&labels) += end.since_request_open;
                });
            },

            Event::StreamResponseFail(ref res, ref fail) => {
                // TODO: do we care about the failure's error code here?
                let labels = Arc::new(ResponseLabels::new(res, None));
                self.update(|metrics| {
                    *metrics.response_total(&labels) += 1;
                    *metrics.response_duration(&labels) += fail.since_response_open;
                    *metrics.response_latency(&labels) += fail.since_request_open;
                });
            },

            Event::TransportOpen(_) | Event::TransportClose(_, _) => {
                // TODO: we don't collect any metrics around transport events.
            },
        };
    }
}


// ===== impl Serve =====

impl Serve {
    fn new(metrics: &Arc<Mutex<Metrics>>) -> Self {
        Serve { metrics: metrics.clone() }
    }
}

impl HyperService for Serve {
    type Request = HyperRequest;
    type Response = HyperResponse;
    type Error = hyper::Error;
    type Future = FutureResult<Self::Response, Self::Error>;

    fn call(&self, req: Self::Request) -> Self::Future {
        if req.path() != "/metrics" {
            return future::ok(HyperResponse::new()
                .with_status(StatusCode::NotFound));
        }

        let body = {
            let metrics = self.metrics.lock()
                .expect("metrics lock poisoned");
            format!("{}", *metrics)
        };
        future::ok(HyperResponse::new()
            .with_header(ContentLength(body.len() as u64))
            .with_header(ContentType::plaintext())
            .with_body(body))
    }
}


// ===== impl RequestLabels =====

impl<'a> RequestLabels {
    fn new(req: &ctx::http::Request) -> Self {
        let outbound_labels = if req.server.proxy.is_outbound() {
            Some(OutboundLabels {
                // TODO: when #429 is done, add appropriate destination label.
                ..Default::default()
            })
        } else {
            None
        };

        let authority = req.uri
            .authority_part()
            .map(http::uri::Authority::to_string)
            .unwrap_or_else(String::new);

        RequestLabels {
            outbound_labels,
            authority,
            ..Default::default()
        }
    }
}

impl fmt::Display for RequestLabels {

    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "authority=\"{}\",", self.authority)?;
        if let Some(ref outbound) = self.outbound_labels {
            write!(f, "direction=\"outbound\"{comma}{dst}",
                comma = if !outbound.is_empty() { "," } else { "" },
                dst = outbound
            )?;
        } else {
            write!(f, "direction=\"inbound\"")?;
        }

        Ok(())
    }

}


// ===== impl OutboundLabels =====

impl OutboundLabels {
    fn is_empty(&self) -> bool {
        self.namespace.is_none() && self.dst.is_none()
    }
}

impl fmt::Display for OutboundLabels {

    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            OutboundLabels { namespace: Some(ref ns), dst: Some(ref dst) } =>
                 write!(f, "dst_namespace=\"{}\",dst_{}", ns, dst),
            OutboundLabels { namespace: None, dst: Some(ref dst), } =>
                write!(f, "dst_{}", dst),
            OutboundLabels { namespace: Some(ref ns), dst: None, } =>
                write!(f, "dst_namespace=\"{}\"", ns),
            OutboundLabels { namespace: None, dst: None, } =>
                write!(f, ""),
        }
    }

}

impl fmt::Display for PodOwner {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            PodOwner::Deployment(ref s) =>
                write!(f, "deployment=\"{}\"", s),
            PodOwner::Job(ref s) =>
                write!(f, "job=\"{}\",", s),
            PodOwner::ReplicaSet(ref s) =>
                write!(f, "replica_set=\"{}\"", s),
            PodOwner::ReplicationController(ref s) =>
                write!(f, "replication_controller=\"{}\"", s),
        }
    }
}

// ===== impl ResponseLabels =====

impl ResponseLabels {
    fn new(rsp: &ctx::http::Response,grpc_status_code: Option<u32>) -> Self {
        let request_labels = RequestLabels::new(&rsp.request);
        ResponseLabels {
            request_labels,
            status_code: rsp.status.as_u16(),
            grpc_status_code,
        }
    }
}

impl fmt::Display for ResponseLabels {

    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{},status_code=\"{}\"",
            self.request_labels,
            self.status_code
        )?;
        if let Some(ref status) = self.grpc_status_code {
            write!(f, "grpc_status_code=\"{}\"", status)?;
        }

        Ok(())
    }

}
