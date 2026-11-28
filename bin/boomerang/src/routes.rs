/*
 * Copyright (c) Mihir Paldhikar
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the “Software”), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED “AS IS”, WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 */
use crate::postgres;
use database::SqliteDatabase;
use http::{HttpCall, HttpStatus, json, route};

#[route("/health", method = GET)]
async fn health_route(call: &HttpCall) {
    call.response()
        .text("boomerang is running")
        .status_code(HttpStatus::Ok)
        .send()
        .await
}

#[route("/sync", method = GET)]
async fn initial_sync(call: &HttpCall) {
    let pg = postgres();
    let pg_schema = pg.schema();

    let sq = match SqliteDatabase::new(pg_schema).await {
        Ok(instance) => instance,
        Err(_) => {
            return call
                .response()
                .json(json!({
                    "code": "SQLITE_INIT_FAILED",
                    "error": "Something went wrong while initializing sqlite"
                }))
                .status_code(HttpStatus::Ok)
                .send()
                .await;
        }
    };

    let bytes = match sq.bytes().await {
        Ok(b) => b,
        Err(_) => {
            return call
                .response()
                .json(json!({
                    "code": "SQLITE_EXPORT_FAILED",
                    "error": "something went wrong while exporting sqlite"
                }))
                .status_code(HttpStatus::Ok)
                .send()
                .await;
        }
    };

    call.response()
        .download(bytes, "sqlite.db", "application/octet-stream")
        .send()
        .await
}
