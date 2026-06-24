;;; Stage 2 macrolet tests — exercise local macro expanders.

;; 1. Basic backquote expander.
(format t "1: ~a~%" (macrolet ((fudge (z) `(* ,z ,z))) (fudge 5)))   ; 25

;; 2. Two local macros, one calls a function, body uses both.
(format t "2: ~a~%"
        (macrolet ((sq (z) `(* ,z ,z))
                   (twice (z) `(+ ,z ,z)))
          (+ (sq 4) (twice 10))))                                      ; 16 + 20 = 36

;; 3. Local macro shadows a global macro of the same name.
(defmacro shad (x) `(list :global ,x))
(format t "3a global: ~a~%" (shad 1))                                 ; (:GLOBAL 1)
(format t "3b local:  ~a~%"
        (macrolet ((shad (x) `(list :local ,x))) (shad 2)))           ; (:LOCAL 2)
(format t "3c global again: ~a~%" (shad 3))                           ; (:GLOBAL 3)

;; 4. Local macro visible while expanding a GLOBAL macro that itself
;;    expands into a call of the local macro (the ANSI ch5 pattern).
(defmacro calls-babbit (n) `(babbit ,n))
(format t "4: ~a~%"
        (macrolet ((babbit (z) `(+ ,z ,z)))
          (calls-babbit 5)))                                           ; 10

;; 5. Nested macrolet — inner shadows outer.
(format t "5: ~a~%"
        (macrolet ((m (x) `(list :outer ,x)))
          (macrolet ((m (x) `(list :inner ,x)))
            (m 7))))                                                   ; (:INNER 7)

;; 6. macrolet expander with &rest.
(format t "6: ~a~%"
        (macrolet ((mlist (&rest xs) `(list ,@xs)))
          (mlist 1 2 3)))                                              ; (1 2 3)

;; 7. The exact ANSI ch5 symbol-macrolet form (Stage 1 regression).
(format t "7: ~a~%"
        (let ((x (list 10 20 30)))
          (symbol-macrolet ((y (car x)) (z (cadr x)))
            (setq y (1+ z) z (1+ y))
            (list x y z))))                                            ; ((21 22 30) 21 22)

(format t "all-done~%")
