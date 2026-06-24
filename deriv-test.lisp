(require 'deriv)

(format t "?? trivial ??~%")
(format t "  d/dx [x]       = ~S~%" (deriv 'x 'x))
(format t "  d/dx [5]       = ~S~%" (deriv 5 'x))
(format t "  d/dx [y]       = ~S~%" (deriv 'y 'x))

(format t "~%?? linear ??~%")
(format t "  d/dx [x+y]     = ~S~%" (deriv '(+ x y) 'x))
(format t "  d/dx [3x+5]    = ~S~%" (deriv '(+ (* 3 x) 5) 'x))

(format t "~%?? product rule ??~%")
(format t "  d/dx [x*y]     = ~S~%" (deriv '(* x y) 'x))
(format t "  d/dx [x*x]     = ~S~%" (deriv '(* x x) 'x))

(format t "~%?? power rule ??~%")
(format t "  d/dx [x^3]     = ~S~%" (deriv '(expt x 3) 'x))
(format t "  d/dx [x^2 + 1] = ~S~%" (deriv '(+ (expt x 2) 1) 'x))

(format t "~%?? trig ??~%")
(format t "  d/dx [sin x]   = ~S~%" (deriv '(sin x) 'x))
(format t "  d/dx [cos x]   = ~S~%" (deriv '(cos x) 'x))
(format t "  d/dx [sin(2x)] = ~S~%" (deriv '(sin (* 2 x)) 'x))

(format t "~%?? combinations ??~%")
(format t "  d/dx [x sin x] = ~S~%" (deriv '(* x (sin x)) 'x))
(format t "  d/dx [e^(2x)]  = ~S~%" (deriv '(exp (* 2 x)) 'x))
(format t "  d/dx [log x]   = ~S~%" (deriv '(log x) 'x))

(format t "~%?? second derivative (apply twice) ??~%")
(format t "  d/dx  [x^3]    = ~S~%" (deriv '(expt x 3) 'x))
(format t "  d?/dx? [x^3]   = ~S~%" (deriv (deriv '(expt x 3) 'x) 'x))
