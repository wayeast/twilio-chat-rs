# Goal
In this step, we want to call a number on our phone and hear audio from an mp3 file on
our filesystem being played back to us.

This will get our application connected to Twilio, and give us an introduction to using
the Twilio API.

# Prerequisites
- _Twilio account and phone number_: For testing, a free Twilio account and a free
  (temporary) phone number will be sufficient.
- _ngrok account_: You will need a way to make your application accessible to Twilio.
  This can be accomplished in a number of different ways, but my personal preference
  is to run the application on my laptop and expose it with
  [ngrok](https://ngrok.com/).  Again, a free account will suffice
  to get this project working.

Instructions on how to set these up is beyond the scope of this tutorial, but there is
good documentation available on the internet:
- [Set up a free Twilio phone number](https://www.twilio.com/docs/usage/tutorials/how-to-use-your-free-trial-account)
  will show you how to set up and use a free Twilio account and phone number.  For the
  first few steps of this project (until we want to send SMS summaries), a free number
  will work just fine.
- [Getting started with ngrok](https://ngrok.com/docs/getting-started/) will show you how
  start ngrok running on your local machine and how to connect to an application
  running on your local machine with a public url.  Pro tip: for this application, you will want
  to have `ngrok` listen on port 3000.
- There is much that one can do with a Twilio number; for this
  application we will be building and application that supports a Twilio voice webhook.
  Finding the right documentation for this is not easy, but start
  [here](https://www.twilio.com/docs/voice/tutorials/how-to-respond-to-incoming-phone-calls),
  find a tutorial that speaks to your comfort zone, and follow instructions for configuring
  Twilio to respond to calls with a webhook that points to your ngrok setup.


# Discussion
Our main interest at this stage is in building an application that can respond to incoming
calls to our Twilio number programmatically.  Twilio supports this by allowing callers to
interact with an application we build via a webhook (see notes in prerequisites above).
The logic flow will look like this:

    ________________                     _________                          __________________
    |               |                    |        |                         |                 |
    |               |-- 1. Place call -->|        |-- 2. POST to webhook -->|                 |
    |  Human Caller |                    | Twilio |                         | Our application |
    |               |<- 4. Respond to ---|        |<--  3. Send Twiml  -----|                 |
    |_______________|      Caller        |________|                         |_________________|

You will notice the introduction of a funny-looking word in step 3) --
 "[Twiml](https://www.twilio.com/docs/voice/twiml)."  Twiml is an instruction set for Twilio
for handling voice calls, encoded as XML.  One of the simplest Twiml instructions is "Say."
For example,

```xml
<Say>Hello.</Say>
```

instructs TWilio to, well, _say_ "Hello" in response to an incoming call.  "Say" is what
Twilio calls a _verb_; Twiml verbs must be wrapped inside a Twiml "Response" tag in
order to constitute valid Twiml.  So, for example, a webhook application would have to
respond to a request from Twilio with a complete XML document like the following:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<Response>
  <Say>Hello.</Say>
</Response>
```

One characteristic of the Twilio call handler, which will be important for understanding
future design decisions, is that a Twiml Response describes a _complete_ response to an
incoming call.  If our application's webhook sent back the above Twiml in step 3), then
in step 4) Twilio would say "Hello" and immediately disconnect the call.  Later, we will
be looking at ways to provide more dynamic interaction with a caller, but for now let's
just get water through pipes.

While it's pretty cool to be able to get Twilio to say anything at all to a caller, we
will eventually want to provide our own audio to be played for a caller.  One (very
simplistic) way to do this is to provide a link to an `mp3` audio file, and use the
Twiml
"[Play](https://www.twilio.com/docs/voice/twiml/play)" verb to have that audio played
back to a connected caller.  The Play verb wraps a publicly-available URL from which
can be fetched `mp3` audio.  This is what we
will be doing here.  In the following steps, we will first begin to build data structures
in Rust that serialize to Twiml XML.  Next, we will create two HTTP handlers: an
audio streaming handler
that reads `mp3` bytes from a file on our local filesystem and streams them back in a
response, and a webhook that constructs a simple Twiml response with a Play verb
pointing to our audio streaming handler.

### 1. XML-able Rust structs
The Twiml response that we want to be able to send in our webhook to
Twilio looks like this example from Twilio:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<Response>
    <Play>https://api.twilio.com/cowbell.mp3</Play>
</Response>
```

This can be represented with 1) the content of a Play tag

```rust
struct PlayAction {
    url: String,
}
```

2) an enum representing the verbs we support (this may also include a `Say` variant
if we wanted to support having Twilio say "Hello"...)

```rust
enum ResponseAction {
    Play(PlayAction),
}
```

and 3) a Response wrapper

```rust
struct Response {
    actions: Vec<ResponseAction>,
}
```

### 2. An audio streaming endpoint handler
The Twiml Play verb needs a URL that responds to a GET request by sending back raw `mp3`
audio bytes.  If we were to do a `wget` on the same URL, we should find it would have
fetched a complete copy of an `mp3` file.  In our implementation, we allow the caller
to specify a filename in the query path.  The following code tries to open a file
specified by the caller in pre-set base directory, then send it back as a byte stream.

```rust
async fn play_handler(
    Path(id): Path<String>,
    State(app_state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let actual = app_state.base_file_dir.join(id);
    let f = File::open(actual).await.unwrap();
    let stream = ReaderStream::new(f);
    let stream_body = StreamBody::new(stream);

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "audio/mpeg".parse().unwrap());

    (headers, stream_body)
}
```

### 3. A Twiml webhook handler
Once we can provide a URL that streams `mp3` audio bytes, we can wrap that URL in a
Play verb.  Our response will replace "https://api.twilio.com/cowbell.mp3" with something that
points to our own audio streaming endpoint.




# TODO:
- why two handlers?
  - start handler to instruct twilio on how to handle call
  - play handler to stream mp3 bytes for twilio to play directly
- mention of getting serde to serialize xml, picture of what twilio responses look like
- we save ourselves the trouble of hardcoding an ngrok host by using the axum host extractor

- as an exercise for the reader, how would you get twilio to speak a greeting before playing the audio file?
