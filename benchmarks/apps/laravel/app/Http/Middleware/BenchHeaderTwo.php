<?php

namespace App\Http\Middleware;

use Closure;
use Illuminate\Http\Request;
use Symfony\Component\HttpFoundation\Response;

/**
 * Middleware 2 of the five the contract requires on `/middleware`. Each one
 * is a separate class so the request really does travel through five frames of
 * Laravel's pipeline, not one class registered five times.
 */
class BenchHeaderTwo
{
    public function handle(Request $request, Closure $next): Response
    {
        $response = $next($request);

        $response->headers->set('x-bench-2', 'ok');

        return $response;
    }
}
