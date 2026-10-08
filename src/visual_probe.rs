//! An explicit experiment: HTTP acceptance alone never establishes image support.
use anyhow::Result;
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{io::Cursor, time::{Duration, Instant}};

#[derive(Debug, thiserror::Error)]
#[error("{error:#}")]
pub struct VisualModelFailure {
    pub manifest: Value,
    #[source]
    pub error: anyhow::Error,
}

#[derive(Debug, thiserror::Error)]
#[error("model returned HTTP {status}: {body}")]
pub struct HttpModelFailure { pub status:u16, pub body:String }

pub fn route_fingerprint(url: &str, model: &str) -> String {
    crate::visual_artifacts::hash(format!("{}\n{model}", url.trim_end_matches('/')).as_bytes())
}

fn challenge() -> Result<(Vec<u8>, Value)> {
    // Randomized pixels, with no labels, alt text or expected answer in the prompt.
    let seed = crate::visual_artifacts::hash(crate::visual_artifacts::new_id().as_bytes());
    let palette = [("red", [220, 35, 45]), ("green", [30, 165, 60]),
        ("blue", [30, 75, 220]), ("yellow", [245, 210, 20]),
        ("purple", [150, 45, 200]), ("orange", [245, 130, 20])];
    let mut order: Vec<usize> = (0..6).collect();
    for i in (1..6).rev() { order.swap(i, seed.as_bytes()[i] as usize % (i + 1)); }
    let mut img = image::RgbImage::from_pixel(600, 400, image::Rgb([255, 255, 255]));
    let mut cells = Vec::new();
    for (i, index) in order.into_iter().enumerate() {
        let (color, rgb) = palette[index];
        let count = 1 + seed.as_bytes()[i + 8] as u32 % 4;
        let x = (i as u32 % 3) * 200; let y = (i as u32 / 3) * 200;
        for dy in 10..190 { for dx in 10..190 { img.put_pixel(x + dx, y + dy, image::Rgb(rgb)); } }
        for dot in 0..count {
            let cx = x + 40 + dot * 40; let cy = y + 100;
            for dy in -12i32..=12 { for dx in -12i32..=12 {
                if dx * dx + dy * dy <= 144 { img.put_pixel((cx as i32 + dx) as u32, (cy as i32 + dy) as u32, image::Rgb([0, 0, 0])); }
            } }
        }
        cells.push(json!({"color":color,"dots":count}));
    }
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgb8(img).write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)?;
    Ok((bytes, json!({"cells":cells})))
}

pub(crate) fn explicitly_rejects_images(status: u16, body: &str) -> bool {
    if !matches!(status, 400 | 415 | 422) { return false; }
    let text = body.to_lowercase();
    (text.contains("image") || text.contains("vision") || text.contains("multimodal") || text.contains("图片"))
        && ["not support", "unsupported", "does not accept", "text-only", "text only", "不支持"].iter().any(|word| text.contains(word))
}

pub async fn probe(client: &reqwest::Client, route: &crate::visual_artifacts::VisualRoute) -> Result<Value> {
    let (bytes, expected) = challenge()?;
    let body = json!({"model":route.model,"stream":false,"max_tokens":1000,"messages":[
        {"role":"user","content":[
            {"type":"text","text":"Inspect the attached image. It has a 2-row, 3-column grid of colored tiles with black circular dots. Return ONLY JSON: {\"cells\":[{\"color\":\"red|green|blue|yellow|purple|orange\",\"dots\":integer}, ...]}. Read left to right across the top row, then the bottom row. Count the dots in each tile. If you cannot see the image, say so; do not guess."},
            {"type":"image_url","image_url":{"url":format!("data:image/png;base64,{}",STANDARD.encode(&bytes)),"detail":"high"}}
        ]}
    ]});
    // Inspect the finalized transport, not an earlier artifact receipt.
    let encoded = body["messages"][0]["content"][1]["image_url"]["url"].as_str().unwrap();
    let delivered = STANDARD.decode(encoded.split_once(',').unwrap().1)?;
    anyhow::ensure!(delivered == bytes, "probe request did not contain the challenge pixels");
    let mut report = json!({"probe_id":crate::visual_artifacts::new_id(),"provider":route.provider,"model":route.model,
        "route_fingerprint":route_fingerprint(&route.url,&route.model),"checked_at":crate::visual_artifacts::now(),
        "capability":"unknown","status":"unconfirmed","expected":expected,
        "request":{"contains_image_content":true,"image_count":1,"image_hash":crate::visual_artifacts::hash(&delivered),"image_bytes":delivered.len(),"width":600,"height":400,"request_bytes":body.to_string().len()},
        "http_status":null,"response_body":null,"error":null});
    let started = Instant::now();
    let result = tokio::time::timeout(Duration::from_secs(60), async {
        let response = client.post(format!("{}/chat/completions",route.url.trim_end_matches('/')))
            .bearer_auth(&route.api_key).json(&body).send().await.map_err(|e|e.without_url())?;
        let status = response.status().as_u16();
        let text = response.text().await.map_err(|e|e.without_url())?;
        Ok::<_,reqwest::Error>((status,text))
    }).await;
    report["elapsed_ms"] = json!(started.elapsed().as_millis() as u64);
    match result {
        Ok(Ok((status, text))) => {
            report["http_status"] = json!(status); report["response_body"] = json!(text);
            if (200..300).contains(&status) {
                let response: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                let raw = response.pointer("/choices/0/message/content").and_then(Value::as_str).unwrap_or("");
                let answer: Value = serde_json::from_str(raw.trim().trim_start_matches("```json").trim_end_matches("```").trim()).unwrap_or(Value::Null);
                report["answer"] = answer.clone();
                if answer["cells"] == expected["cells"] {
                    report["capability"] = json!("supported"); report["status"] = json!("verified");
                } else { report["error"] = json!("HTTP request succeeded, but the image-only challenge was not answered correctly; image capability remains unconfirmed"); }
            } else {
                report["error"] = json!(format!("model returned HTTP {status}: {text}"));
                if explicitly_rejects_images(status, &text) {
                    report["capability"] = json!("unsupported"); report["status"] = json!("rejected");
                }
            }
        },
        Ok(Err(error)) => report["error"] = json!(format!("{:#}",anyhow::Error::new(error))),
        Err(_) => report["error"] = json!("image capability probe timed out after 60 seconds"),
    }
    Ok(report)
}

/// Read HTTP errors before discarding the response so every role retains the provider's reason.
pub async fn successful_response(response: reqwest::Response) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() { return Ok(response); }
    let text = response.text().await.map_err(|e|e.without_url())?;
    Err(HttpModelFailure {status:status.as_u16(),body:text}.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn probe_requires_pixels_and_correct_answer_and_preserves_rejections() {
        use axum::{Json, extract::State, routing::post};
        async fn reply(State(mode): State<u8>, Json(body): Json<Value>) -> axum::response::Response {
            use axum::response::IntoResponse;
            let url = body["messages"][0]["content"][1]["image_url"]["url"].as_str().unwrap();
            let img = image::load_from_memory(&STANDARD.decode(url.split_once(',').unwrap().1).unwrap()).unwrap().to_rgb8();
            assert_eq!(img.dimensions(), (600,400));
            if mode == 2 { return (axum::http::StatusCode::BAD_REQUEST,"this model does not support image input").into_response(); }
            if mode == 3 { return (axum::http::StatusCode::UNAUTHORIZED,"invalid credential").into_response(); }
            let palette = [([220,35,45],"red"),([30,165,60],"green"),([30,75,220],"blue"),([245,210,20],"yellow"),([150,45,200],"purple"),([245,130,20],"orange")];
            let cells: Vec<_> = (0..6).map(|i| {
                let x = i%3*200; let y=i/3*200;
                let color = palette.iter().find(|(rgb,_)| img.get_pixel(x+20,y+20).0 == *rgb).unwrap().1;
                let dots=(0..4).filter(|dot|img.get_pixel(x+40+dot*40,y+100).0 == [0,0,0]).count();
                json!({"color":color,"dots":dots})
            }).collect();
            let content = if mode==1 {"{}".to_owned()}else{json!({"cells":cells}).to_string()};
            Json(json!({"choices":[{"message":{"content":content}}]})).into_response()
        }
        for mode in 0..4 {
            let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();let addr=listener.local_addr().unwrap();
            let server=tokio::spawn(async move {axum::serve(listener,axum::Router::new().route("/chat/completions",post(reply)).with_state(mode)).await.unwrap()});
            let route=crate::visual_artifacts::VisualRoute{provider:"fixture".into(),model:"m".into(),url:format!("http://{addr}"),api_key:"secret".into()};
            let report=probe(&reqwest::Client::new(),&route).await.unwrap();
            assert_eq!(report["capability"],match mode {0=>"supported",2=>"unsupported",_=>"unknown"},"{report}");
            assert!(!report.to_string().contains("secret"));assert!(!report.to_string().contains("base64"));
            if mode>=2 {assert!(report["response_body"].as_str().unwrap().contains(if mode==2 {"image input"}else{"invalid credential"}));}
            server.abort();
        }
    }
}
