# Overview
This project allows you to interact with ChatGPT as though you were phoning
a friend.  Simply pick up a phone, dial a number, ask questions, whisper
sweet nothings, and hear ChatGPT's responses spoken back to you!  As an added
bonus, when you hang up you receive SMS summaries of ChatGPT's responses!

The app connects a number of different component pieces:
1. [Twilio](https://www.twilio.com): Twilio provides an API for handling phone calls.
2. [Deepgram](https://deepgram.com/): ChatGPT only handles text prompts; Deepgram Speech Recognition
   converts the caller's voice to text.
3. [ChatGPT](https://chat.openai.com): The app uses the OpenAI API to send prompts to ChatGPT
4. [Google Text-to-Speech](https://cloud.google.com/text-to-speech): voice responses are synthesized from ChatGPT text
   responses for playing back to the caller.

Since connecting all of these pieces is complex, I've tried to organize this
project into branches that each represent a simple, contained step toward
our ultimate goal.
