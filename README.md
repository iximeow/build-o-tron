# build-o-tron (name pending)

this is the most bespoke CI situation you've ever heard of. this is the system backing green checkmarks for most of the yaxpeax repos on github, among others.

the only known instance of anyone using this is me, at [ci.butactuallyin.space](https://ci.butactuallyin.space)

notably:
* CI configs are lua
* anyone can run CI jobs (if the server is willing to talk to you)
* (soon) the CI runner should be able to run locally in a `watch` manner - CI builds should be testable before reaching CI
* i explicitly intend this system to support non-github triggers
  [ ] say, post-recv hooks in other git repos 
  [ ] or a patched cgit to show build statuses 
  [ ] or a cron job for non-build-oriented work that should none the less be repeatable 
* i expect this system to have first-class support for capturing metrics from builds and reporting on changes
  [ ] and yes, i expect build-o-tron to double as a performance testing environment 
  [ ] including replaying a history of build jobs on a new runner to establish performance baselines and differences 
* it knows how to send emails nagging me about builds and their outcomes. other tools can do this too i'm sure, but i can make these useful.
[ ] in the future, figure out how to do certain kinds of limited deployments after successful goodfile executions. probably goodfile extensions to do those deployments, so success implies successful deployments. 

this will probably grow into a general task scheduler, if my interest stays here. i've built at least one of those before.

look, i really really really do not want to run jenkins.
