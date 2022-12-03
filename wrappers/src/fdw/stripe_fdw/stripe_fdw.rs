use pgx::log::PgSqlErrorCode;
use pgx::prelude::Timestamp;
use pgx::JsonB;
use reqwest::{self, header, Url};
use reqwest_middleware::{ClientBuilder, ClientWithMiddleware};
use reqwest_retry::{policies::ExponentialBackoff, RetryTransientMiddleware};
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use time::OffsetDateTime;

use supabase_wrappers::prelude::*;

fn create_client(api_key: &str) -> ClientWithMiddleware {
    let mut headers = header::HeaderMap::new();
    let value = format!("Bearer {}", api_key);
    let mut auth_value = header::HeaderValue::from_str(&value).unwrap();
    auth_value.set_sensitive(true);
    headers.insert(header::AUTHORIZATION, auth_value);
    let client = reqwest::Client::builder()
        .default_headers(headers)
        .build()
        .unwrap();
    let retry_policy = ExponentialBackoff::builder().build_with_max_retries(3);
    ClientBuilder::new(client)
        .with(RetryTransientMiddleware::new_with_policy(retry_policy))
        .build()
}

fn extract_to_rows(
    resp_body: &str,
    obj_key: &str,
    common_cols: Vec<(&str, &str)>,
    tgt_cols: &Vec<String>,
) -> (Vec<Row>, Option<String>, Option<bool>) {
    let mut result = Vec::new();
    let value: JsonValue = serde_json::from_str(resp_body).unwrap();
    let objs = value
        .as_object()
        .and_then(|v| v.get(obj_key))
        .and_then(|v| v.as_array())
        .unwrap();
    let mut cursor: Option<String> = None;

    for obj in objs {
        let mut row = Row::new();

        // extract common columns
        for (col_name, col_type) in &common_cols {
            if tgt_cols.iter().any(|c| c == col_name) {
                let cell = obj
                    .as_object()
                    .and_then(|v| v.get(*col_name))
                    .and_then(|v| match *col_type {
                        "i64" => v.as_i64().map(|a| Cell::I64(a)),
                        "string" => v.as_str().map(|a| Cell::String(a.to_owned())),
                        "timestamp" => v.as_i64().map(|a| {
                            let dt = OffsetDateTime::from_unix_timestamp(a).unwrap();
                            let ts = Timestamp::try_from(dt).unwrap();
                            Cell::Timestamp(ts)
                        }),
                        _ => None,
                    });
                row.push(col_name, cell);
            }
        }

        // put all properties into 'attrs' JSON column
        if tgt_cols.iter().any(|c| c == "attrs") {
            let attrs = serde_json::from_str(&obj.to_string()).unwrap();
            row.push("attrs", Some(Cell::Json(JsonB(attrs))));
        }

        result.push(row);
    }

    // get last object's id as cursor
    if let Some(last_obj) = objs.last() {
        cursor = last_obj
            .as_object()
            .and_then(|v| v.get("id"))
            .and_then(|v| v.as_str())
            .map(|v| v.to_owned());
    }

    // get 'has_more' attribute
    let has_more = value
        .as_object()
        .and_then(|v| v.get("has_more"))
        .and_then(|v| v.as_bool());

    (result, cursor, has_more)
}

fn pushdown_quals(url: &mut Url, quals: &Vec<Qual>, fields: Vec<&str>) {
    for qual in quals {
        for field in &fields {
            if qual.field == *field && qual.operator == "=" && !qual.use_or {
                match &qual.value {
                    Value::Cell(cell) => match cell {
                        Cell::String(s) => {
                            url.query_pairs_mut().append_pair(field, &s);
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
        }
    }
}

#[wrappers_meta(
    version = "0.1.1",
    author = "Supabase",
    website = "https://github.com/supabase/wrappers/tree/main/wrappers/src/fdw/stripe_fdw"
)]
pub(crate) struct StripeFdw {
    rt: Runtime,
    base_url: Url,
    client: Option<ClientWithMiddleware>,
    scan_result: Option<Vec<Row>>,
}

impl StripeFdw {
    pub fn new(options: &HashMap<String, String>) -> Self {
        let base_url = options
            .get("api_url")
            .map(|t| t.to_owned())
            .unwrap_or("https://api.stripe.com/v1/".to_string());
        let client = match options.get("api_key") {
            Some(api_key) => Some(create_client(&api_key)),
            None => require_option("api_key_id", options)
                .and_then(|key_id| get_vault_secret(&key_id))
                .and_then(|api_key| Some(create_client(&api_key))),
        };

        StripeFdw {
            rt: create_async_runtime(),
            base_url: Url::parse(&base_url).unwrap(),
            client,
            scan_result: None,
        }
    }

    fn build_url(
        &self,
        obj: &str,
        quals: &Vec<Qual>,
        page_size: i64,
        cursor: &Option<String>,
    ) -> Option<Url> {
        let mut url = self.base_url.join(&obj).unwrap();

        // pushdown quals for balance transactions
        // ref: https://stripe.com/docs/api/balance_transactions/list
        if obj == "balance_transactions" {
            pushdown_quals(&mut url, quals, vec!["payout", "type"]);
        }

        // pushdown quals for charges
        // ref: https://stripe.com/docs/api/charges/list
        if obj == "charges" {
            pushdown_quals(&mut url, quals, vec!["customer"]);
        }

        // pushdown quals for customers
        // ref: https://stripe.com/docs/api/customers/list
        if obj == "customers" {
            pushdown_quals(&mut url, quals, vec!["email"]);
        }

        // pushdown quals for invoices
        // ref: https://stripe.com/docs/api/invoices/list
        if obj == "invoices" {
            pushdown_quals(&mut url, quals, vec!["customer", "status", "subscription"]);
        }

        // pushdown quals for payment intents
        // ref: https://stripe.com/docs/api/payment_intents/list
        if obj == "payment_intents" {
            pushdown_quals(&mut url, quals, vec!["customer"]);
        }

        // pushdown quals for subscriptions
        // ref: https://stripe.com/docs/api/subscriptions/list
        if obj == "subscriptions" {
            pushdown_quals(&mut url, quals, vec!["customer", "price", "status"]);
        }

        // add pagination parameters except for 'balance' object
        if obj != "balance" {
            url.query_pairs_mut()
                .append_pair("limit", &format!("{}", page_size));
            if let Some(ref cursor) = cursor {
                url.query_pairs_mut().append_pair("starting_after", cursor);
            }
        }

        Some(url)
    }

    // convert response body text to rows
    fn resp_to_rows(
        &self,
        obj: &str,
        resp_body: &str,
        tgt_cols: &Vec<String>,
    ) -> (Vec<Row>, Option<String>, Option<bool>) {
        match obj {
            "balance" => extract_to_rows(
                resp_body,
                "available",
                vec![("amount", "i64"), ("currency", "string")],
                tgt_cols,
            ),
            "balance_transactions" => extract_to_rows(
                resp_body,
                "data",
                vec![
                    ("id", "string"),
                    ("amount", "i64"),
                    ("currency", "string"),
                    ("description", "string"),
                    ("fee", "i64"),
                    ("net", "i64"),
                    ("status", "string"),
                    ("type", "string"),
                    ("created", "timestamp"),
                ],
                tgt_cols,
            ),
            "charges" => extract_to_rows(
                resp_body,
                "data",
                vec![
                    ("id", "string"),
                    ("amount", "i64"),
                    ("currency", "string"),
                    ("customer", "string"),
                    ("description", "string"),
                    ("invoice", "string"),
                    ("payment_intent", "string"),
                    ("status", "string"),
                    ("created", "timestamp"),
                ],
                tgt_cols,
            ),
            "customers" => extract_to_rows(
                resp_body,
                "data",
                vec![("id", "string"), ("email", "string")],
                tgt_cols,
            ),
            "invoices" => extract_to_rows(
                resp_body,
                "data",
                vec![
                    ("id", "string"),
                    ("customer", "string"),
                    ("subscription", "string"),
                    ("status", "string"),
                    ("total", "i64"),
                    ("currency", "string"),
                    ("period_start", "timestamp"),
                    ("period_end", "timestamp"),
                ],
                tgt_cols,
            ),
            "payment_intents" => extract_to_rows(
                resp_body,
                "data",
                vec![
                    ("id", "string"),
                    ("customer", "string"),
                    ("amount", "i64"),
                    ("currency", "string"),
                    ("payment_method", "string"),
                    ("created", "timestamp"),
                ],
                tgt_cols,
            ),
            "subscriptions" => extract_to_rows(
                resp_body,
                "data",
                vec![
                    ("id", "string"),
                    ("customer", "string"),
                    ("currency", "string"),
                    ("current_period_start", "timestamp"),
                    ("current_period_end", "timestamp"),
                ],
                tgt_cols,
            ),
            _ => {
                report_error(
                    PgSqlErrorCode::ERRCODE_FDW_TABLE_NOT_FOUND,
                    &format!("'{}' object is not implemented", obj),
                );
                (Vec::new(), None, None)
            }
        }
    }
}

macro_rules! report_fetch_error {
    ($err:ident) => {{
        report_error(
            PgSqlErrorCode::ERRCODE_FDW_ERROR,
            &format!("fetch failed: {}", $err),
        );
        return;
    }};
}

impl ForeignDataWrapper for StripeFdw {
    fn begin_scan(
        &mut self,
        quals: &Vec<Qual>,
        columns: &Vec<String>,
        _sorts: &Vec<Sort>,
        limit: &Option<Limit>,
        options: &HashMap<String, String>,
    ) {
        let obj = if let Some(name) = require_option("object", options) {
            name.clone()
        } else {
            return;
        };

        if let Some(client) = &self.client {
            let page_size = 100; // maximum page size limit for Stripe API
            let page_cnt = if let Some(limit) = limit {
                if limit.count == 0 {
                    return;
                }
                (limit.offset + limit.count) / page_size + 1
            } else {
                // if no limit specified, fetch all records
                i64::MAX
            };
            let mut page = 0;
            let mut result = Vec::new();
            let mut cursor: Option<String> = None;

            while page < page_cnt {
                // build url
                let url = self.build_url(&obj, quals, page_size, &cursor);
                if url.is_none() {
                    return;
                }
                let url = url.unwrap();

                // make api call
                match self.rt.block_on(client.get(url).send()) {
                    Ok(resp) => match resp.error_for_status() {
                        Ok(resp) => {
                            let body = self.rt.block_on(resp.text()).unwrap();
                            let (rows, starting_after, has_more) =
                                self.resp_to_rows(&obj, &body, columns);
                            if rows.is_empty() {
                                break;
                            }
                            result.extend(rows);
                            if let Some(has_more) = has_more {
                                if !has_more {
                                    break;
                                }
                            }
                            cursor = starting_after;
                        }
                        Err(err) => report_fetch_error!(err),
                    },
                    Err(err) => report_fetch_error!(err),
                }

                page += 1;
            }

            self.scan_result = Some(result);
        }
    }

    fn iter_scan(&mut self) -> Option<Row> {
        if let Some(ref mut result) = self.scan_result {
            if !result.is_empty() {
                return result.drain(0..1).last();
            }
        }
        None
    }

    fn end_scan(&mut self) {
        self.scan_result.take();
    }
}
