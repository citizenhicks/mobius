use super::*;
use futures_util::io::AsyncReadExt;
use gpui::http_client::{HttpRequestExt, RedirectPolicy};
use std::io::{Read, Write};
use std::net::TcpListener;

#[test]
fn image_requests_preserve_redirects_headers_status_and_streamed_bytes() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        for response in [
            "HTTP/1.1 302 Found\r\nLocation: /image\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            "HTTP/1.1 404 Not Found\r\nContent-Type: image/png\r\nContent-Length: 5\r\nConnection: close\r\n\r\nimage",
        ] {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = [0; 4096];
            let read = stream.read(&mut request).unwrap();
            let request = std::str::from_utf8(&request[..read]).unwrap();
            assert!(
                request.to_lowercase().contains("user-agent: mobius/"),
                "{request}"
            );
            stream.write_all(response.as_bytes()).unwrap();
        }
    });
    let client = ImageClient::new().unwrap();
    client.runtime.block_on(async {
        let mut response = client
            .get(
                &format!("http://{address}/redirect"),
                AsyncBody::empty(),
                true,
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 404);
        assert_eq!(response.headers()["content-type"], "image/png");
        let mut body = Vec::new();
        response.body_mut().read_to_end(&mut body).await.unwrap();
        assert_eq!(body, b"image");
        let unsupported = Request::post(format!("http://{address}/"))
            .body(AsyncBody::empty())
            .unwrap();
        assert_eq!(
            client
                .send(unsupported)
                .await
                .err()
                .expect("unsupported request")
                .to_string(),
            "the image client only accepts empty GET requests"
        );
        for policy in [
            None,
            Some(RedirectPolicy::NoFollow),
            Some(RedirectPolicy::FollowLimit(1)),
        ] {
            let request = Request::get(format!("http://{address}/"))
                .when_some(policy, HttpRequestExt::follow_redirects)
                .body(AsyncBody::empty())
                .unwrap();
            assert_eq!(
                client
                    .send(request)
                    .await
                    .err()
                    .expect("unsupported request")
                    .to_string(),
                "the image client requires redirects to be enabled"
            );
        }
    });
    server.join().unwrap();
}
