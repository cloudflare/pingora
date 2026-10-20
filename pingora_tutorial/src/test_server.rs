use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

// Function that starts the test web server
pub fn start_test_server(address: &str) -> std::io::Result<()> {
    let listener = TcpListener::bind(address)?;
    println!("Test server running on {}", address);

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                thread::spawn(move || {
                    handle_test_client(stream);
                });
            }
            Err(e) => eprintln!("Failed to accept connection: {}", e),
        }
    }
    Ok(())
}

// Function to handle each client and respond with a "Hello. You made it!" page
fn handle_test_client(mut stream: TcpStream) {
    let mut buffer = [0; 512];
    if stream.read(&mut buffer).is_ok() {
        // HTTP response with "Hello. You made it!" message
        let response = r#"HTTP/1.1 200 OK
Content-Type: text/html

<!DOCTYPE html>
<html>
<head><title>Welcome</title></head>
<body>
    <h1>Hello. You made it!</h1>
</body>
</html>"#;

        // Write the response back to the client
        if stream.write_all(response.as_bytes()).is_err() {
            eprintln!("Failed to send response");
        }
    }
}
    