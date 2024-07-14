# reg-interface

A utility to run locally on staff registration computers to decode and send
barcodes to a backend, and permit printing using local printers.

## Configuration

A config.toml must be present in the working directory or available at a path
specified in the `REG_INTERFACE_CONFIG_PATH` environment variable.

### Printer

A printer can be specified in the print section.

```toml
[print]
printer_uri = "ipp://localhost:631/printers/printer_name"
```

Print jobs can be created by sending a print action through a connection target
that supports bidirectional communication.

```json
{
  "action": "print",
  "url": "https://example.com/test.pdf"
}
```

The PDF will be streamed from the provided URL and sent to the printer.

### Scanners

Any number of serial and USB HID (using the point of sale protocol) barcode
scanners can be configured. Each scanner can enable or disable decoders or
specify transformers using [Rhai] to modify the scanned data or override default
behavior before transmitting it to a connection target.

```toml
[[scanner.inputs]]
path = "/dev/tty.usbmodemXXXX"
baud_rate = 115200
decoders = ["aamva", "generic"]
connection_targets = ["test1"]
open_urls = true
transformer = """
switch SCANNED_DATA.data_type {
    "aamva" => {
        #{
            name: #{
                first: SCANNED_DATA.name?.first,
                last: SCANNED_DATA.name?.family
            },
            birthday: SCANNED_DATA.date_of_birth
        }
    }
}
"""

[[scanner.inputs]]
vendor_id = 1504
product_id = 1536
usage_id = 3072
usage_page = 65283
decoders = ["url", "generic"]
```

When using a transformer block, the scanned data is available in the
`SCANNED_DATA` constant. The `data_type` property will always be set to the
name of the decoder that processed the scan.

When the link decoder is enabled and a barcode is decoded as a link, and if the
`open_urls` option is set to true or the transformer sets `OPEN_URL` to true,
the URL will be opened automatically on the local machine.

If a transformer returns a unit type, the original data will be used instead.

By default all decoded inputs will be sent to all connection targets, but this
can be filtered by setting the `connection_targets` option. For more control,
the transformer can set the `CONNECTION_TARGETS` variable to an array of target
names.

[Rhai]: https://rhai.rs

### Connections

After a barcode has been processed, it will be sent to all connection targets
that support the decoder.

```toml
[[connection.targets]]
name = "test1"
connection_type = "websocket"
url = "ws://localhost:8080"
allow_actions = true

[[connection.targets]]
name = "test2"
supported_decoders = ["url", "generic"]
connection_type = "http"
url = "http://localhost:8080"
user_agent = "reg-interface"
headers = { "x-secret" = "some-value" }

[[connection.targets]]
name = "test3"
connection_type = "mqtt"
url = "mqtt://localhost?client_id=12345"
allow_actions = true
publish_topic = "client/12345/scan"
action_topic = "client/12345/action"
```

If the target supports bidirectional communication (WebSockets or MQTT), you
may enable `allow_actions` to process actions coming from the target.
