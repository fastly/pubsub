# Pub/Sub

Pub/Sub is a publish/subscribe messaging broker that runs on Fastly Compute.

Features:

* Supports millions of concurrent subscribers, even to the same topics.
* Subscribe via Server-Sent Events or MQTT.
* Publish via HTTP POST or MQTT.
* JWTs for access control.
* Optional message durability using Fastly KV Store.

# Requirements

In addition to Compute, this app depends on the following Fastly components:

* Fanout (for long-lived push connections)
* Config Store (for non-secret configuration values)
* Secret Store (for secret configuration values)
* KV Store (for token signing keys and message storage)

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
* Only QoS level 0 is supported (though messages can still be reliably delivered; see [Durability](#durability)).
* Wildcard subscriptions are not supported.

### Durability

The last message published to each topic can be stored for reliable delivery. Both the publisher and subscriber must opt-in to this behavior.

To enable durable messaging:

1. Create a KV Store and link it to the app under the name "messages".
2. When subscribing, indicate interest in durable messages. For HTTP, include a `durable=true` query parameter. For MQTT, set the "retain handling" field to 0 in the `SUBSCRIBE` packet.
3. When publishing, indicate that the message should be retained. For HTTP, include a `retain=true` query parameter. For MQTT, set the "retain" flag in the `PUBLISH` packet.

It is also possible to set an expiration on the message. For HTTP, include a `ttl` query parameter set to a number of seconds. For MQTT, set the "message expiry interval" field in the `PUBLISH` packet. By default, messages don't expire.

If a retained message is published but no subscribers have requested durable messages, delivery of the message will still be attempted but without any delivery guarantee.

For MQTT, durability is implemented as retained messages rather than a non-zero QoS level. This is because publishing a new message essentially revokes the durability of any previous message, which may be insufficient for QoS 1. However, the latest retained message is still at-least-once delivered until it is replaced or expires.

Only being able to store the last message may seem limiting, but there are some benefits:

* Storing messages indefinitely becomes practical. You can retain messages without an expiration and use them to serve initial content.
* No worry about bombarding subscribers with a large message backlog.

The feature is best used for message streams where the latest message supersedes all previous messages. If you need to send a stream of changes that can only be reconciled by receiving every message, you may want to publish a hint or version number and have the subscriber fetch the actual changes out of band.
