use std::collections::HashSet;
use std::time::Duration;

use serde_json::Value;

const PAGE_SIZE: u32 = 1000;
const MAX_PAGES: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub id: String,
}

pub struct Client {
    base_url: String,
    api_key: String,
    agent: ureq::Agent,
}

impl Client {
    pub fn new(base_url: &str, api_key: &str) -> Result<Self, String> {
        let base_url = normalize_base(base_url)?;
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(60)))
            .build()
            .into();
        Ok(Self {
            base_url,
            api_key: api_key.to_string(),
            agent,
        })
    }

    pub fn list_images(&self, album_id: &str) -> Result<Vec<Image>, String> {
        self.check_key()?;
        check_id(album_id)?;
        let album = self.get_json(&format!("/api/albums/{album_id}"))?;
        // Immich before v3 returned `assets` here. v3 removed it.
        if let Some(images) = images_from_album(&album) {
            return Ok(images);
        }
        self.list_via_search(album_id)
    }

    pub fn download_preview(&self, asset_id: &str) -> Result<Vec<u8>, String> {
        self.check_key()?;
        check_id(asset_id)?;
        let mut response = self
            .agent
            .get(format!("{}/api/assets/{asset_id}/thumbnail", self.base_url))
            .query("size", "preview")
            .header("x-api-key", &self.api_key)
            .call()
            .map_err(|err| format!("download preview: {err}"))?;
        response
            .body_mut()
            .read_to_vec()
            .map_err(|err| format!("download preview: {err}"))
    }

    fn list_via_search(&self, album_id: &str) -> Result<Vec<Image>, String> {
        collect_pages(|page| {
            let body = self.search_page(album_id, page)?;
            page_from_search(&body)
        })
    }

    fn search_page(&self, album_id: &str, page: u32) -> Result<Value, String> {
        let mut response = self
            .agent
            .post(format!("{}/api/search/metadata", self.base_url))
            .header("x-api-key", &self.api_key)
            .send_json(serde_json::json!({
                "albumIds": [album_id],
                "type": "IMAGE",
                "withDeleted": false,
                "page": page,
                "size": PAGE_SIZE,
            }))
            .map_err(|err| format!("list album: {err}"))?;
        response
            .body_mut()
            .read_json()
            .map_err(|err| format!("list album: {err}"))
    }

    fn get_json(&self, path: &str) -> Result<Value, String> {
        let mut response = self
            .agent
            .get(format!("{}{path}", self.base_url))
            .header("x-api-key", &self.api_key)
            .call()
            .map_err(|err| format!("list album: {err}"))?;
        response
            .body_mut()
            .read_json()
            .map_err(|err| format!("list album: {err}"))
    }

    fn check_key(&self) -> Result<(), String> {
        if self.api_key.is_empty() {
            Err("API key is empty".to_string())
        } else {
            Ok(())
        }
    }
}

fn normalize_base(url: &str) -> Result<String, String> {
    let url = url.trim().trim_end_matches('/');
    let url = url
        .strip_suffix("/api")
        .unwrap_or(url)
        .trim_end_matches('/');
    if url.is_empty() {
        return Err("Immich URL is empty".to_string());
    }
    if !(url.starts_with("https://")) {
        return Err("Immich URL must start with https://".to_string());
    }
    Ok(url.to_string())
}

fn check_id(id: &str) -> Result<(), String> {
    let ok = !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
    if ok {
        Ok(())
    } else {
        Err(format!("invalid id: {id}"))
    }
}

fn images_from_album(body: &Value) -> Option<Vec<Image>> {
    let assets = body.get("assets")?.as_array()?;
    Some(ids_to_images(image_ids(assets)))
}

fn page_from_search(body: &Value) -> Result<Page, String> {
    let items = body
        .get("assets")
        .and_then(|assets| assets.get("items"))
        .and_then(|items| items.as_array())
        .ok_or_else(|| "search response has no assets.items".to_string())?;
    Ok(Page {
        images: ids_to_images(image_ids(items)),
        next: next_page(body),
    })
}

struct Page {
    images: Vec<Image>,
    next: Option<u32>,
}

fn collect_pages(mut fetch: impl FnMut(u32) -> Result<Page, String>) -> Result<Vec<Image>, String> {
    let mut page = 1u32;
    let mut seen = HashSet::from([page]);
    let mut images = Vec::new();
    loop {
        if seen.len() > MAX_PAGES {
            return Err("album has too many pages".to_string());
        }
        let current = fetch(page)?;
        if current.images.is_empty() {
            break;
        }
        images.extend(current.images);
        let Some(next) = current.next else {
            break;
        };
        if !seen.insert(next) {
            break;
        }
        page = next;
    }
    Ok(images)
}

fn ids_to_images(ids: Vec<String>) -> Vec<Image> {
    ids.into_iter().map(|id| Image { id }).collect()
}

fn image_ids(assets: &[Value]) -> Vec<String> {
    assets
        .iter()
        .filter_map(|asset| {
            if asset.get("type").and_then(Value::as_str) != Some("IMAGE") {
                return None;
            }
            asset.get("id").and_then(Value::as_str).map(str::to_string)
        })
        .collect()
}

fn next_page(body: &Value) -> Option<u32> {
    match body.get("assets")?.get("nextPage")? {
        Value::String(text) => text.parse().ok(),
        Value::Number(number) => u32::try_from(number.as_u64()?).ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_url_must_be_http_and_trimmed() {
        assert_eq!(
            normalize_base("  https://immich.example/api/  ").unwrap(),
            "https://immich.example"
        );
        match Client::new("", "key") {
            Err(err) => assert_eq!(err, "Immich URL is empty"),
            Ok(_) => panic!("empty URL should fail"),
        }
        assert!(Client::new("immich.example", "key").is_err());
        assert!(Client::new("http://immich.local", "key").is_err());
    }

    #[test]
    fn album_assets_keep_image_order() {
        let body = serde_json::json!({
            "assets": [
                { "id": "a", "type": "IMAGE" },
                { "id": "b", "type": "VIDEO" },
                { "id": "c", "type": "IMAGE" }
            ]
        });
        let images = images_from_album(&body).unwrap();
        assert_eq!(
            images,
            vec![
                Image {
                    id: "a".to_string()
                },
                Image {
                    id: "c".to_string()
                }
            ]
        );
    }

    #[test]
    fn album_without_assets_is_not_a_list() {
        let body = serde_json::json!({ "assetCount": 2 });
        assert!(images_from_album(&body).is_none());
    }

    #[test]
    fn search_pages_follow_next_page_once() {
        let pages = [
            serde_json::json!({
                "assets": {
                    "items": [{ "id": "a", "type": "IMAGE" }],
                    "nextPage": "2"
                }
            }),
            serde_json::json!({
                "assets": {
                    "items": [
                        { "id": "b", "type": "VIDEO" },
                        { "id": "c", "type": "IMAGE" }
                    ],
                    "nextPage": "2"
                }
            }),
        ];
        let images = collect_pages(|page| page_from_search(&pages[page as usize - 1])).unwrap();
        assert_eq!(
            images,
            vec![
                Image {
                    id: "a".to_string()
                },
                Image {
                    id: "c".to_string()
                }
            ]
        );
    }

    #[test]
    fn empty_search_page_ends_the_list() {
        let body = serde_json::json!({ "assets": { "items": [], "nextPage": "2" } });
        let images = collect_pages(|_page| page_from_search(&body)).unwrap();
        assert!(images.is_empty());
    }

    #[test]
    fn next_page_accepts_string_or_number() {
        let as_string = serde_json::json!({ "assets": { "nextPage": "3" } });
        let as_number = serde_json::json!({ "assets": { "nextPage": 4 } });
        let absent = serde_json::json!({ "assets": { "nextPage": null } });
        assert_eq!(next_page(&as_string), Some(3));
        assert_eq!(next_page(&as_number), Some(4));
        assert_eq!(next_page(&absent), None);
    }

    #[test]
    fn ids_reject_path_characters() {
        assert!(check_id("abc-123").is_ok());
        assert!(check_id("../etc").is_err());
        assert!(check_id("").is_err());
    }
}
