use git_async::file_system::FileSystemError;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{Request, RequestInit, RequestMode, Response};

async fn send(url: &str) -> Result<Response, FileSystemError> {
    let window = web_sys::window()
        .ok_or_else(|| FileSystemError::Other(Box::new("no window".to_string())))?;
    let opts = RequestInit::new();
    opts.set_method("GET");
    opts.set_mode(RequestMode::Cors);
    let request = Request::new_with_str_and_init(url, &opts)
        .map_err(|e| FileSystemError::Other(Box::new(e.as_string().unwrap_or_default())))?;
    let resp_value = JsFuture::from(window.fetch_with_request(&request))
        .await
        .map_err(|e| FileSystemError::Other(Box::new(e.as_string().unwrap_or_default())))?;
    resp_value
        .dyn_into::<Response>()
        .map_err(|_| FileSystemError::Other(Box::new("not a Response".to_string())))
}

pub(crate) async fn fetch_bytes(url: &str) -> Result<Vec<u8>, FileSystemError> {
    let resp = send(url).await?;
    if resp.status() == 404 {
        return Err(FileSystemError::NotFound(Box::new(url.to_string())));
    }
    let array_buffer = JsFuture::from(
        resp.array_buffer()
            .map_err(|e| FileSystemError::Other(Box::new(e.as_string().unwrap_or_default())))?,
    )
    .await
    .map_err(|e| FileSystemError::Other(Box::new(e.as_string().unwrap_or_default())))?;
    Ok(js_sys::Uint8Array::new(&array_buffer).to_vec())
}

pub(crate) async fn fetch_text(url: &str) -> Result<String, FileSystemError> {
    let bytes = fetch_bytes(url).await?;
    String::from_utf8(bytes).map_err(|e| FileSystemError::Other(Box::new(e.to_string())))
}
