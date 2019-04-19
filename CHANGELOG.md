Version 0.2.3 (2019-04-18)
==========================

* Fix bug encountered while processing queue when a requester has dropped
  while popping read lock requests.

Version 0.2.2 (2019-03-08)
==========================

* Fix `Guard::unlock` to actually unlock qutex.