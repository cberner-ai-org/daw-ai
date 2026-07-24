DAW-AI
------

This project is an AI powered DAW that is intended for people interested in making music
who do not have the skills to use a commercial DAW.

It should be simple to use and the interface should rely heavily on AI powered interactions.

### UI

There is an Export button that renders the whole track to a .wav file and initiates a download of it.

#### Multi-user support

There is no authentication. Users are identified with a cookie. Each user gets their own project. And
multiple users working on their own projects concurrently is supported.

#### AI Mode
"AI Mode" is the primary UI and it is a timeline view of the track. The user can then select a portion of the track
with their mouse and enter a prompt for the AI describing the change to be made. This might be something
as simple as "increase volume" or as complex as "insert a sick drop here". The AI then makes those changes.

After submitting a change request, the submit button becomes an interrupt button.

Double-clicking (or long-pressing on mobile) on the track selects the entire track.

There is a session history list of all the actions the agent took. Clicking on one moves the project back to that
state, so that the user can play and inspect it. The session history does not rollback, allowing the user to navigate
forward again.

#### Advanced Mode
There is also an advanced view which the user can open that shows the details of the instruments, effects,...etc
that the agent has implemented for the track. This should be similar to a traditional DAW, but prioritize simple
interfaces over super powerful tools. It allows the user to directly edit any of the tracks, and also create
or delete entire tracks of the sound graph.

MIDI clips are shown with a standard MIDI visualization and editor.

Instruments, modulators, and effects are shown in an associated sound graph. Click a node in the graph shows
a side pane that displays the relevant parameters and settings.

These two views are separate tabs, each filling most of the screen, and there is a prominent tab near the top to
switch between AI Mode, Advanced, and Debug.

#### Debug

There is also a third tab "Debug" which is a debugging pane showing error information, and other information
that is useful to a coding assistant. The information is easy for the user to copy and paste into an
external coding assistant, if they need help debugging issues in DAW AI itself. It can be assumed that
the user and coding assistant have access to the machine DAW AI is deployed on, to read additional logs...etc.

### Sound tools

The following sound tools should be implemented and available in the advanced view of the UI and also to the AI model.

These are all implemented in the DAW AI backend. The client-side JS contains a basic editor to modify the sound graph and view it,
but all execution of it is in the backend server process.

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
My also be tempo sync'ed, or configured to trigger off a MIDI note event

#### Routing
Instruments, effects, and modulators can be connected into a sound graph.

Edges in the sound graph carry one of the following types:

* MIDI events: Timed musical events such as note-on, note-off, pitch, velocity, and other performance controls.
* Audio signal: Mono or stereo digital audio.
* Control signal: A time-varying numeric value used to control an Instrument or Effect parameter.

Connections must have compatible types:

* MIDI Clip -> Instrument or Modulator: MIDI events
* Instrument -> Effect: Audio signal
* Effect -> Effect or Output: Audio signal
* Modulator -> Instrument or Effect parameter: Control signal

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

The backend is written in Rust. The client code should be responsive and the UI should work on mobile or a desktop
browser.

The default AI used is Gemini 3.6 Flash. The user must provide an API key in ~/gemini_creds.txt or a similar file.
It can also be specified as an environment variable.

Codex is also available as an alternative. For Codex to work the user must authenticate via the Codex CLI
and then the local credentials are used. All the DAW AI tooling integration is optimized for Gemini,
but the same tools should be available to Codex and registered with it. Because Codex cannot take
audio as input, the "listening" tool that renders audio should save it to a local file and return
the path to Codex for it to analyze.

There is a dropdown menu on the Debug tab to switch between Codex and Gemini.

Since Gemini is best at writing code and config files, the internal synth and other tools that DAW-AI uses should
be represented a way that is friendly for Gemini:
* The sound graph should be stored in a file on disk that Gemini can edit directly
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
