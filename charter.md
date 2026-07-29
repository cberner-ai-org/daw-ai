DAW-AI
------

This project is an AI powered DAW that is intended for people interested in making music
who do not have the skills to use a commercial DAW.

It should be simple to use and the interface should rely heavily on AI powered interactions.

### UI

There is an Export button that renders the whole track to a .wav file and initiates a download of it.

There are two separate tabs, each filling most of the screen, and there is a prominent tab near the top to
switch between AI Mode and Debug.

#### Multi-user support

There is no authentication. Users are identified with a cookie. Each user gets their own project. And
multiple users working on their own projects concurrently is supported.

#### AI Mode
"AI Mode" is the primary UI and it is a timeline view of the track. The user can then select a portion of the track
with their mouse and enter a prompt for the AI describing the change to be made. This might be something
as simple as "increase volume" or as complex as "insert a sick drop here". The AI then makes those changes.

The UI shows a track for each instrument, and the MIDI notes that are key-zoned to that instrument

There is also a small spectrum analyzer on the left of the timeline for each track, which animates during playback.

After submitting a change request, the submit button becomes an interrupt button.

Double-clicking (or long-pressing on mobile) on the track selects the entire track.

There is a session history list of all the actions the agent took. Clicking on one moves the project back to that
state, so that the user can play and inspect it. The session history does not rollback, allowing the user to navigate
forward again.

#### Debug

There is also a tab "Debug" which is a debugging pane showing error information, and other information
that is useful to a coding assistant. The information is easy for the user to copy and paste into an
external coding assistant, if they need help debugging issues in DAW AI itself. It can be assumed that
the user and coding assistant have access to the machine DAW AI is deployed on, to read additional logs...etc.

### Sound tools

The following sound tools should be implemented and available to the AI model.

These are all implemented in the DAW AI backend, not client-side.

The MIDI clips are built into DAW AI, but everything else: Instrument, Effects, Modulation, Routing primarily relies on
those implementations in Surge XT. DAW AI adds only a minimal layer on top to expose them to Gemini, persist settings...etc.

#### MIDI Clip
Contains notes, including their timing, duration, pitch, and velocity.

#### Instrument:
Produces sound from MIDI events.

For the current MVP, this should be a basic implementation, which relying on [Surge XT](https://surge-synthesizer.github.io/) as the synthesizer
and exposes basic presets and parameters. Use the official [surge-rs](https://github.com/surge-synthesizer/surge-rs) Rust bindings.
They are alpha quality, so if there are critical bugs, it is ok to vendor it and patch the bugs.

#### Effect
Processes sound produced by an instrument, such as a filter, distortion, compressor, delay, or reverb, and exposes configurable parameters. May be chained with previous Effect.

Like Instrument Surge XT is the sound effects engine and it should expose the Surge native effects, including those from presets
that the model uses.

#### Modulator / Automation
Generates time-varying control values—such as envelopes, LFOs, or arbitrary curves—which can control any Instrument or Effect parameter.
May also be tempo sync'ed, or configured to trigger off a MIDI note event


#### Routing

MIDI clips uses key zones, where ranges of notes are configured to route to an instrument instance.
Multiple key zones, routing to different instruments, per MIDI clip are supported. Multiple zones may route
to the same instrument, and zones may overlap.

One or more MIDI clips may be used in the arrangement.

Instruments, effects, and modulators can be connected into a sound graph.

A MIDI clip may contain notes routed to different Instruments

Effect routing is a DAG composed of serial effect chains, parallel sends, and summing buses.
The implementation maps this to Surge XT’s fixed Scene A/Scene B insert, send, and global-effect topology.
Arbitrary routing cycles are not required.

Modulators form a many-to-many graph and connect to controlable parameters. Cycles in the graph are not supported.
The implementation should map to Surge's modulation system.

The final output of all Instruments (w/ optional Effect chain) is mixed together

### AI editing

The AI edits the sound graph. It is able to use any of the tools, and may construct the graph iteratively over many modifications.

The AI should first form a musical plan based on the user’s request, the selected region, and the existing composition,
and then produce the corresponding sound-graph changes. The system instructions should include concrete examples and
concise guidance connecting musical concepts to the available sound tools.

### Error logs

The backend server logs errors and warnings to stderr. If the client code encounters an error it sends
it to the backend server to be included in the logs.

### Vetoed Implementations

The implementation MUST NOT hardcode niche sound tools such as a dubstep "drop" tool. All the tools should
be simple primitives that the AI (or user) uses to build the sound.

The implementation MUST NOT use Web Audio. It must be a backend built on Surge XT that runs in the server process.

### Implementation

The project is currently in alpha status. When implementing changes there is no need to maintain backward compatibility.
DO NOT include extra code to support legacy project files

The interface should be a local webserver with no authentication required. It should run on port 8888 by default.

The backend is written in Rust and the sound engine uses Surge XT. Minimize the amount of custom audio code in DAW AI.
DAW AI's sound engine should be a thin orchestration layer on top of Surge XT, and it should NOT override defaults,
or implement its own processing effects. The only exception is when Surge XT is not well suited to handle the task,
such as the final mixing of tracks, exporting of .wav files...etc.
The client code should be responsive and the UI should work on mobile or a desktop browser.

The AI used is Gemini 3.6 Flash. The user must provide an API key in ~/gemini_creds.txt or a similar file.
It can also be specified as an environment variable.


Since Gemini is best at writing code and config files, the internal synth and other tools that DAW-AI uses should
be represented a way that is friendly for Gemini:
* The sound graph should be stored in a file on disk
* Additionally, tools should be provided that are registered with Gemini and that make the
  modifications to the sound graph and return useful error messages to Gemini.
* Gemini may perform multiple edits to fulfil a request, which are shown to the user incrementally
* There is an audio rendering tool that allows Gemini to render part of the sound graph.
  It is then returned to Gemini as audio input

#### Gemini loop
Gemini is told to operate in an implementation loop. It should:
* Make edits to the sound graph
* Listen to the audio
* Consider whether the request has been completed
* Repeat, if necessary

DAW AI MUST NOT limit the number of iterations or tools calls, except with a long timeout on the whole request.

DAW AI should display incremental updates to the progress bar as the AI progresses.

#### Gemini sessions
Sessions should be logged to disk for debugging purposes, and listed on the Debug tab by date and timestamp.

### Deployment

The expected deployment is either as a local webserver, or on a private network where a gateway handles authentication.
To support the latter case, the DAW AI server must not restrict the hostname in requests.
Also to support the reverse proxy case, the server must be designed for reasonable timeouts and other characters
appropriate to deploy it behind nginx.
