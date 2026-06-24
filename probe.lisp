(format t "start~%")
(setq *load-path* (cons "E:/CL/NewCormanLisp/Lisp/Library/" *load-path*))
(format t "before streams~%") (require 'streams)    (format t "streams ok~%")
(format t "before conditions~%") (require 'conditions) (format t "conditions ok~%")
(format t "before loop~%") (require 'loop)       (format t "loop ok~%")
(format t "before sequences~%") (require 'sequences) (format t "sequences ok~%")
(format t "before events~%") (require 'events)     (format t "events ok~%")
(format t "before hot-reload~%") (require 'hot-reload) (format t "hot-reload ok~%")
nil
