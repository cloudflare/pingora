# Pingora Tutorial

The Cargo.lock and Cargo.toml are provided in the Pingora crate if you so choose to use them. Both have been labeled with the prefix of `tutorial_` so you will need to change their names before starting this tutorial. Having that will prevent you from needing to cargo add any crates that may be required to start up any of this code. If you don't want to use that then we will also make sure to point out the exact imports that we're using in each section so that you can install them yourself if you decide.

So with that out of the way, lets begin our Pingora tutorial.

### Introduction

Before we begin building a full proxy server, we need a simple server to test and demonstrate our proxy’s functionality. In this tutorial, we’ll start by setting up a basic test web server in Rust that will display a simple message when accessed. This will help us verify that our proxy server is routing requests correctly. The server has already been built in it's entirety inside of the `pingora_tutorial/src/test_server.rs`. This tutorial will explain the code inside that folder so you better understand it and are able to run or modify the code as you see fit.

Thank you for using Pingora

## Step 1: Start the Test Web Server

To begin, let’s create a basic server function that listens on a specified address and handles incoming client connections. This server will allow us to test the future code we’ll write for the proxy server.

```rust
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
```

### Imports:

```rust
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
```

`std::io::{Read, Write}`: This imports Read and Write, traits that provide reading and writing functionality for streams. We’ll use these to handle data between the client and server.

`std::net::{TcpListener, TcpStream}`: `TcpListener` allows us to listen for incoming TCP connections on a specified address. `TcpStream` represents a connection between a client and the server.

`std::thread`: This imports Rust’s threading capabilities, allowing us to spawn new threads for each client connection to handle multiple clients concurrently.

### Function Declaration:

```rust
pub fn start_test_server(address: &str) -> std::io::Result<()> {
```

`start_test_server` is a function that takes an address parameter of type `&str` (a string slice).
It returns a `Result<(), std::io::Error>`. This indicates that the function might return an `std::io::Error` if there’s a problem, such as if the address is already in use.

### Binding the Listener:

```rust    
let listener = TcpListener::bind(address)?;
```

`TcpListener::bind(address)?` creates a TCP listener that binds to the provided address (e.g., "127.0.0.1:9000"). The `?` operator automatically returns an error if binding fails.

This listener will listen for incoming connections on the specified address.

### Console Output:

```rust 
println!("Test server running on {}", address);
```

Prints a message to the console to indicate that the test server has started successfully and is running on the specified address.

### Handling Incoming Connections:

```rust
for stream in listener.incoming() {
```

`listener.incoming()` is an iterator that yields incoming connections. Each connection is represented as a `TcpStream`, allowing data transfer with a client.
We use a `for` loop to handle each incoming connection individually.

### Match Statement and Threading:

```rust
    match stream {
        Ok(stream) => {
            thread::spawn(move || {
                handle_test_client(stream);
            });
        }
        Err(e) => eprintln!("Failed to accept connection: {}", e),
    }
}
```

`match stream`: We check whether each incoming connection (i.e., each stream) is successful (Ok) or encountered an error (Err).

`Ok(stream)`: If a connection is established successfully, stream represents the `TcpStream` of that connection.

`thread::spawn(move || { handle_test_client(stream); })`: Spawns a new thread for each connection, passing the `TcpStream` to `handle_test_client`.
This enables handling multiple client connections concurrently.

`Err(e)`: If there’s an error accepting a connection, `eprintln!` outputs an error message to the console.

### Return Value:

```rust
    Ok(())
}
```

Returns `Ok(())`, indicating that the function completed successfully if there were no binding errors.

## Step 2: Display a Response Message

To confirm that our server is working correctly, we’ll add a function to respond with a simple HTML message, “Hello. You made it!” This will help us know when we have successfully reached the test server.

```rust
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
```

### Code: Displaying a Response Message

```rust
// Function to handle each client and respond with a "Hello. You made it!" page
fn handle_test_client(mut stream: TcpStream) {
```

### Function Declaration:

`handle_test_client` takes a mutable `TcpStream` called stream as input, representing an open connection with a client.
This function will read the client’s request and write back a simple HTML response.

### Buffer Initialization:

```rust
let mut buffer = [0; 512];
```

`let mut buffer = [0; 512];` creates a buffer, a fixed-size array of 512 bytes, to temporarily hold data read from the client.
512 bytes is generally enough to capture an HTTP request from the client.

### Reading from the Client:

```rust
    if stream.read(&mut buffer).is_ok() {
```

`stream.read(&mut buffer).is_ok()`: Reads data from the client into buffer. The `is_ok()` check ensures that reading was successful.
We’re not inspecting the content of the request in this example; we simply need to know the client connected to respond with our message.

### HTTP Response:

```rust
        let response = r#"HTTP/1.1 200 OK
Content-Type: text/html

<!DOCTYPE html>
<html>
<head><title>Welcome</title></head>
<body>
    <h1>Hello. You made it!</h1>
</body>
</html>"#;
```

`let response = ...`: Defines an HTTP response as a raw string (r#"...#").
The response includes:
`Status Line: HTTP/1.1 200 OK` indicates a successful HTTP response.
`Headers: Content-Type: text/html` informs the client that the response is HTML.
`HTML Content:` Displays a simple message, `<h1>Hello. You made it!</h1>`, in an HTML page structure.

This message will be displayed in the client’s browser when they successfully reach the server.

### Writing the Response:

```rust
if stream.write_all(response.as_bytes()).is_err() {
    eprintln!("Failed to send response");
        }
    }
}
```

`stream.write_all(response.as_bytes())`: Writes the response back to the client as bytes.

`response.as_bytes()` converts the response string into a byte slice for sending over the network.

`is_err()`: If an error occurs while sending the response, an error message is printed to the console with `eprintln!`.

## Onto the next step
You have now created a rust server that will listen and respond to connections. So lets take this server and use it, as well as copies of it, to explore the rest of Pingora.

``` Some link here getting you to the next step ```
