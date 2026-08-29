<?php

use Illuminate\Foundation\Application;
use Illuminate\Foundation\Configuration\Exceptions;
use Illuminate\Foundation\Configuration\Middleware;
use Illuminate\Http\Request;
use Illuminate\Support\Facades\Route;

return Application::configure(basePath: dirname(__DIR__))
    ->withRouting(
        commands: __DIR__.'/../routes/console.php',
        // `using:` instead of `web:` so the benchmark routes are registered
        // without the `web` middleware group. The contract puts a stack on
        // `/middleware` and on nothing else, and sessions, cookie encryption
        // and CSRF on `/plaintext` would be work no other app in the
        // comparison is doing.
        using: function (): void {
            Route::group([], base_path('routes/web.php'));
        },
    )
    ->withMiddleware(function (Middleware $middleware): void {
        //
    })
    ->withExceptions(function (Exceptions $exceptions): void {
        $exceptions->shouldRenderJsonWhen(
            fn (Request $request) => $request->is('api/*') || $request->expectsJson(),
        );
    })->create();
