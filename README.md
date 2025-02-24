# reg-interface

A utility to perform registration-related activites on staff machines.

The core features are:

- Scanning, decoding, and transforming results of barcode and ID scanners
- Enqueuing print jobs to the local machine from a remote server

## Configuration

A config.toml must be present in the working directory or available at a path
specified in the `REG_INTERFACE_CONFIG_PATH` environment variable.

### Decoders

The included decoders have a number of configuration options. Many are enabled
by default, but those requiring specific setup such as mDL and SHC are not.

```toml
[decoder]
enabled_decoders = ["aamva", "mdl", "shc", "url", "generic"]

[decoder.mdl]
input_name = "mdl"
timeout = 120
certificates_path = "mdl-certificates"
nfc_connstring = "pn532_uart:/dev/ttyACM0"

[decoder.mdl.request_elements."org.iso.18013.5.1"]
given_name = false
family_name = false
birth_date = false

[decoder.shc]
skip_updates = false
prohibit_lookups = false
cache_dir = "shc-cache"

[decoder.url]
open_urls = false
```

### Inputs

Any number of serial and USB HID (using the point of sale protocol) barcode
scanners can be configured.

```toml
[[input]]
name = "serial-scanner"
path = "/dev/tty.usbmodemXXXX"
baud_rate = 115200

[[input]]
name = "hid-scanner"
vendor_id = 1504
product_id = 1536
usage_id = 3072
usage_page = 65283
```

### Connections

After a barcode has been processed, it will be transformed and then sent to the
relevant connections.

```toml
[[connection]]
name = "http"
connection_type = "http"
url = "http://localhost:8080"
user_agent = "reg-interface"
headers = { "x-secret" = "some-value" }

[[connection]]
name = "mqtt"
connection_type = "mqtt"
url = "mqtt://user:pass@localhost"
allow_actions = true
publish_topic = "client/12345"
action_topic = "client/12345/action"
```

If the connection supports bidirectional communication (such as MQTT), you
may enable `allow_actions` to process actions coming from the connection.

### Transformations

In order to increase flexibility, a script written using [Rhai] may be used to
transform data between being decoded and before it's sent to connetions.

[Rhai]: https://rhai.rs

```toml
[transform]
template_path = "transform.rhai"

[transform.extra]
device_name = "test"
```

```rhai
// transform.rhai

fn transform(input_name, decoder_type, data) {
  switch decoder_type {
    "AAMVA" => {
      #{
        first: data.name?.first,
        last: data.name?.family,
        dob: data.date_of_birth,
        _interface: #{
          mqtt_topic: `${extra::device_name}/id`
        }
      }
    }
  }
}
```

The transform function may return a map with the `_interface` key set, which can
control some connection-specific behavior. This key is removed before sending
the data to the relevant connections.

For example, setting `mqtt_topic` changes the topic to which the value is sent.
It's also possible to set `targets` to control which connections will be sent
the value.

### Printer

A printer can be configured in the actions section.

```toml
[action.print]
printer_uri = "ipp://localhost:631/printers/printer_name"

[[action.print.attributes]]
name = "landscape"
value = {Boolean = true}
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
