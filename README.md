## Chat project
Here I want to start to have some conversational sensitivity.  This looks like:
- our bot listens to the caller until they pause for a configurable
  POLITENESS_DELAY.  That is, our bot doesn't try to speak until
  POLITENESS_DELAY has passed without the caller talking.
- if the caller talks while our bot is talking, the bot should stop
  talking immediately and listen.
- (???) if our bot is interrupted, it should remember what it was going to say
  to keep context.

This version doesn't actually converse.  It still only echoes what it hears back
to the caller; it just tries to be as polite as a long-distance friend can be.
