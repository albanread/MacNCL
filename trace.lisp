;; Simulate the macro body by hand
(let ((args '((= 1 1) then :yes))
      (xx nil)
      (state :init)
      (else-seen nil)
      (total-col nil)
      (col nil))
  (setq xx (reverse args))
  (format t "xx initial: ~S~%" xx)
  (loop
    (when (null xx)
      (format t "loop ends, state=~A total-col=~S~%" state total-col)
      (return))
    (let* ((tok (car xx))
           (lookat (cond ((not (symbolp tok)) nil)
                         ((eq tok 'then)    'then)
                         ((eq tok 'thenret) 'thenret)
                         ((eq tok 'else)    'else)
                         ((eq tok 'elseif)  'elseif)
                         (t nil))))
      (format t "  tok=~S lookat=~S state=~S~%" tok lookat state)
      (cond
        ((eq state :init)
         (cond
           (lookat (cond ((eq lookat 'thenret) (setq col nil) (setq state :then))
                         (t (error "bad ~A" lookat))))
           (t (setq state :col) (setq col nil) (setq col (cons tok col)))))
        ((eq state :col)
         (cond
           (lookat
            (cond ((eq lookat 'else) (setq state :init) (setq total-col (cons (cons 't col) total-col)))
                  ((eq lookat 'then) (setq state :then))
                  (t (error "bad-col ~A" lookat))))
           (t (setq col (cons tok col)))))
        ((eq state :then)
         (cond
           (lookat (error "wrong place ~A" tok))
           (t (setq state :compl)
              (setq total-col (cons (cons tok col) total-col)))))
        ((eq state :compl)
         (cond
           ((not (eq lookat 'elseif)) (error "missing elseif")))
         (setq state :init))))
    (setq xx (cdr xx))))
