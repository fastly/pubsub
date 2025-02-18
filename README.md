# Pub/Sub

Pub/Sub is a publish/subscribe messaging broker that runs on Fastly Compute. It supports messaging via Server-Sent Events or MQTT, using JWTs for access control.

# Requirements

In addition to Compute, this app depends on the following Fastly components:

* Fanout (for long-lived push connections)
* Config Store (for non-secret configuration values)
* Secret Store (for secret configuration values)
* KV Store (for token signing keys)

# Setup

First, deploy the app:

```sh
fastly compute publish
```

Assuming the fastly.toml file hasn't been modified, the above command will create a new Compute service and set up any related resources.

Enable Fanout on the service:

```sh
fastly products --enable=fanout
```

Add a backend called "self" to your service, for directing Fanout-managed requests back to the service itself. Replace `{DOMAIN}` with the domain selected during deployment:

```sh
fastly backend create --name self --address {DOMAIN} --port 443 --version latest --autoclone
fastly service-version activate --version latest
```

Determine the ID of the "secrets" Secret Store:

```sh
fastly resource-link list --version latest
```

Look for a block of output where the Resource Type is `secret-store` and the Name is whatever name you gave to the "secrets" Secret Store. The Resource ID is the store's ID.

Create a Fastly API token with the ability to publish Fanout messages. Do this by going to the Fastly management panel -> Accounts -> API tokens -> Personal tokens -> Create Token. Select Automation type, `purge_select` scope, limited to the service, and never expiring, and click Create Token. Then save the token in the above store:

```sh
fastly secret-store-entry create -s {STORE_ID} --name publish-token
```

The above command will prompt for the token value, which you can paste in.

# Questions/Comments 

Use the issues for specific code related bugs or features or chat with us on any additional questions on the [Fastly Community Forum](https://community.fastly.com/t/announcing-fastlys-official-pubsub-application/3876). 

# Usage

Clients connect using HTTP or MQTT (over WebSockets). Requests must be authorized using tokens.

### Keys and tokens

In order to work with tokens you first need to create a signing key. Do this by sending a POST to the app's `/admin/keys` endpoint:

```sh
curl -X POST -H "Fastly-Key: $FASTLY_API_TOKEN" https://{DOMAIN}/admin/keys
```

The app will respond with a key ID and value. Note them in a safe place. The value is used for signing JWTs. The ID must be included in the `kid` header field of the JWTs.

The admin API can only be used to create keys. However, keys are saved in the "keys" KV Store and further key management can be done directly with the store.

Once you have a signing key, you can create authorization tokens for subscribers and publishers as needed. Below is an example using Python and the PyJWT library to create a token capable of both subscribing and publishing to the topics "topic1" and "topic2" that lasts 1 hour. Replace `{KEY_ID}` and `{KEY_VALUE}` with your key ID and value.

```py
import jwt
import time

jwt.encode({
    "exp": int(time.time())+3600,
    "x-fastly-read": ["topic1","topic2"],
    "x-fastly-write": ["topic1","topic2"],
}, "{KEY_VALUE}", headers={"kid":"{KEY_ID}"})
```

The `x-fastly-read` and `x-fastly-write` claims indicate the allowed topics for subscribing and publishing, respectively.

### SSE

To subscribe via SSE, make a GET request to the `/events` path of the Compute app, specifying one or more `topic` query parameters as the topics to subscribe to. Include an authentication token with the necessary permissions either in the `Authorization` header (`Bearer` type) or in the `auth` query parameter.

Example subscribe:

```
$ curl \
  -H "Authorization: Bearer $TOKEN" \
  "https://{DOMAIN}/events?topic=topic1&topic=topic2"
```

This establishes a never-ending response body that the client can receive messages over.

Example received message:

```
event: message
data: {"text":"hello world"}
```

If a message's content is valid UTF-8, clients will receive an event of type `message` with the data as-is. Otherwise, clients will receive an event of type `message-base64` with the data Base64-encoded.

### Publishing via HTTP

To publish via HTTP, make a POST request to the `/events` path of the Compute app, specifying one `topic` query parameter as the topic to publish to, along with a token, and message content in the request body. The message content can be anything, including binary data.

Example publish:

```
$ curl \
  -H "Authorization: Bearer $TOKEN" \
  -d '{"text":"hello world"}'
  "https://{DOMAIN}/events?topic=topic1"
```

Messages are delivered to both SSE and MQTT subscribers.

### MQTT

To subscribe or publish via MQTT, make a WebSocket request to `/mqtt` with subprotocol `mqtt`, and use MQTT protocol version 5 over the WebSocket connection. When sending a `CONNECT` packet, include an access token in the password field.

Messages published via MQTT are delivered to both SSE and MQTT subscribers.

Below is an example using MQTT.js:

```js
const mqtt = require("mqtt");

const token = "{token with read/write access to topic1}";

const client = mqtt.connect("wss://{DOMAIN}/mqtt", {
  protocolVersion: 5,
  username: "",
  password: token,
});

client.on("connect", () => {
  console.log("connected");
  client.subscribe("topic1", (err) => {
    if (err) {
      console.log("subscribe failed");
      client.end();
    } else {
      console.log("subscribed");
      client.publish("topic1", "Hello mqtt");
    }
  });
});

client.on("message", (topic, message) => {
  console.log(message.toString());
  client.end();
});
```

Notes & limitations about the MQTT interface:

* Only WebSocket connections are supported, not plain TCP connections.
* Only MQTT protocol version 5 is supported.
* Wildcard subscriptions are not supported.
* MQTT packets received from the client may span multiple WebSocket messages. However, due to the way this app buffers partial content, the combined size of all messages containing partial content is limited to 8,192 bytes. For example, sending a 32kB packet to the server in a single message is fine. Sending a 32kB packet to the server spanning three messages of sizes 4kB, 4kB, and 24kB is also fine. However, sending a packet to the server spanning three messages of sizes 24kB, 4kB, 4kB would fail.
