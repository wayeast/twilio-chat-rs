## Chat project
In which we play back to the caller their words that Deepgram
hears.

This does nothing fancy -- only adds Deepgram to the mix.  Here,
we open a websocket stream to Deepgram; for every streaming response
we get, we wait for three seconds, then send the transcript to Google
for TTS, and send the resulting TTS to Twilio.
